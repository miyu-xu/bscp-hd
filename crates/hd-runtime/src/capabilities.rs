use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use hd_core::{
    ArtifactSelectionV2, COMPONENT_PROTOCOL_VERSION, CONTROL_PROTOCOL_VERSION, CapabilityProbeV2,
    CapabilityStatusV2, DeviceBackendKindV2, DeviceCapabilitiesV2, DeviceCapabilityV2,
    FRAME_PROTOCOL_VERSION, FormalComponentProbeV2, FrameTransportKindV2, GuestKindV2,
    HOST_CERTIFICATION_VERSION, HostCapabilitiesV2, HostCertificationV2, InstanceSpecV2,
    LeaseKindV2, LeaseV2,
};
use hd_platform::{
    DataPaths, FrameInteropProbeV2, architecture_name, configure_managed_command, contain_process,
    executable_name, hypervisor_available as native_hypervisor_available,
    platform_baseline as native_platform_baseline, platform_name, resume_managed_process,
};
use parking_lot::RwLock as ParkingRwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::leases::{
    guest_cpu_capacity, host_cpu_reserve, leased_quantity, requested_cpu_count,
    uses_match_host_cpu_topology,
};
#[cfg(test)]
use crate::required_runtime_disk_bytes;
use crate::{ArtifactResolver, ArtifactTrustStore, ResolvedBundleSetV2, runtime_disk_requirement};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;
const MICRODROID_DEV_BYPASS_ENV: &str = "HD_MICRODROID_DEV_BYPASS";
const MICRODROID_IDENTITY_VERSION: u32 = 2;

fn microdroid_profile() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "hd-microdroid-windows-x86_64-v2"
    } else {
        "hd-microdroid-macos-arm64-v2"
    }
}

/// Reports whether this HD build has a product-supported Microdroid backend. Presentation code
/// must consume this capability instead of inferring support from a browser user agent.
pub fn microdroid_platform_supported() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
        || cfg!(all(target_os = "windows", target_arch = "x86_64"))
}

fn microdroid_product_name() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "vsoc_x86_64"
    } else {
        "vsoc_arm64_only"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicrodroidRuntimeIdentityV2 {
    pub schema_version: u32,
    pub profile: String,
    pub guest_digest: String,
    pub host_digest: String,
}

#[derive(Debug, Clone)]
pub(crate) struct MicrodroidRuntimePaths {
    pub bin_dir: PathBuf,
    pub lib_dir: PathBuf,
    pub product_root: PathBuf,
}

impl MicrodroidRuntimePaths {
    pub fn vm(&self) -> PathBuf {
        self.bin_dir.join(executable_name("vm"))
    }

    pub fn virtmgr(&self) -> PathBuf {
        self.bin_dir.join(executable_name("virtmgr"))
    }

    pub fn crosvm(&self) -> PathBuf {
        self.bin_dir.join(executable_name("crosvm"))
    }

    fn binder_rpc(&self) -> PathBuf {
        #[cfg(windows)]
        let name = "libbinder-rpc.dll";
        #[cfg(not(windows))]
        let name = "libbinder-rpc.1.dylib";
        let in_bin = self.bin_dir.join(name);
        if in_bin.is_file() {
            in_bin
        } else {
            self.lib_dir.join(name)
        }
    }

    fn identity(&self) -> PathBuf {
        std::env::var_os("HD_MICRODROID_IDENTITY_PATH").map_or_else(
            || {
                self.product_root
                    .parent()
                    .unwrap_or(&self.product_root)
                    .join("runtime-identity-v2.json")
            },
            PathBuf::from,
        )
    }
}

pub(crate) fn resolve_microdroid_runtime_paths() -> MicrodroidRuntimePaths {
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let workspace_root = cfg!(debug_assertions)
        .then(|| Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3))
        .flatten()
        .map(Path::to_path_buf);

    let bin_dir = std::env::var_os("HD_MICRODROID_DIST_ROOT")
        .map(PathBuf::from)
        .map(|root| {
            [
                root.join(platform_name()).join("bin"),
                root.join("macos").join("bin"),
                root.join("bin"),
                root.clone(),
            ]
            .into_iter()
            .find(|candidate| candidate.join(executable_name("vm")).is_file())
            .unwrap_or_else(|| root.join("macos").join("bin"))
        })
        .or_else(|| {
            executable_dir
                .as_ref()
                .filter(|directory| directory.join(executable_name("vm")).is_file())
                .cloned()
        })
        .or_else(|| {
            workspace_root.as_ref().map(|root| {
                root.join("out")
                    .join("dist")
                    .join(platform_name())
                    .join("bin")
            })
        })
        .unwrap_or_else(|| PathBuf::from(platform_name()).join("bin"));
    let adjacent_lib = bin_dir
        .parent()
        .map(|parent| parent.join("lib"))
        .filter(|path| path.is_dir());
    let lib_dir = adjacent_lib.unwrap_or_else(|| bin_dir.clone());

    let bundled_product = executable_dir.as_ref().and_then(|directory| {
        let contents = directory.parent()?;
        (directory.file_name().is_some_and(|name| name == "MacOS")).then(|| {
            contents
                .join("Resources")
                .join("products")
                .join("microdroid")
                .join("vsoc_arm64_only")
        })
    });
    let product_root = std::env::var_os("HD_MICRODROID_ARTIFACTS_ROOT")
        .map(PathBuf::from)
        .or_else(|| bundled_product.filter(|path| path.is_dir()))
        .or_else(|| {
            workspace_root.as_ref().and_then(|root| {
                root.parent().map(|workspace| {
                    workspace
                        .join("products")
                        .join("microdroid")
                        .join(microdroid_product_name())
                })
            })
        })
        .unwrap_or_else(|| {
            PathBuf::from("products")
                .join("microdroid")
                .join(microdroid_product_name())
        });

    MicrodroidRuntimePaths {
        bin_dir,
        lib_dir,
        product_root,
    }
}

/// Installs immutable release trust material from a signed application bundle.
///
/// Every bundled certificate is inspected and verified before any destination is changed. This
/// prevents a malformed bundle from leaving a trust root behind after a failed first launch.
pub fn install_bundled_release_materials(paths: &DataPaths) -> Result<Vec<PathBuf>, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve current executable for release bootstrap: {error}"))?;
    let Some(macos_dir) = executable.parent() else {
        return Ok(Vec::new());
    };
    if macos_dir.file_name().is_none_or(|name| name != "MacOS") {
        return Ok(Vec::new());
    }
    let Some(contents) = macos_dir.parent() else {
        return Ok(Vec::new());
    };
    let source_root = contents.join("Resources").join("release");
    match std::fs::symlink_metadata(&source_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "bundled release materials path is not a real directory: {}",
                source_root.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "inspect bundled release materials {}: {error}",
                source_root.display()
            ));
        }
    }

    let source_trust = source_root.join("trusted-keys-v2.json");
    let bundled_trust = ArtifactTrustStore::load(&source_trust)
        .map_err(|error| format!("validate bundled release trust store: {error}"))?;
    let destination_trust = paths.root.join("trusted-keys-v2.json");
    let trust_bytes = hd_platform::read_regular_nofollow_limited(&source_trust, 256 * 1024)
        .map_err(|error| format!("read bundled release trust store: {error}"))?;
    let install_trust = if path_entry_exists(&destination_trust)? {
        let existing = hd_platform::read_regular_nofollow_limited(&destination_trust, 256 * 1024)
            .map_err(|error| format!("read installed release trust store: {error}"))?;
        if existing != trust_bytes {
            return Err(
                "bundled release trust store differs from the installed immutable trust root"
                    .to_owned(),
            );
        }
        false
    } else {
        true
    };

    let certification_plan = prepare_bundled_certifications(paths, &source_root, &bundled_trust)?;
    let mut installed = Vec::with_capacity(certification_plan.len() + usize::from(install_trust));
    for installation in certification_plan {
        hd_platform::write_owner_only(&installation.destination, &installation.bytes).map_err(
            |error| {
                format!(
                    "install bundled certification {}: {error}",
                    installation.destination.display()
                )
            },
        )?;
        installed.push(installation.destination);
    }
    if install_trust {
        hd_platform::write_owner_only(&destination_trust, &trust_bytes)
            .map_err(|error| format!("install bundled release trust store: {error}"))?;
        installed.push(destination_trust);
    }

    Ok(installed)
}

struct CertificationInstallation {
    destination: PathBuf,
    bytes: Vec<u8>,
}

fn prepare_bundled_certifications(
    paths: &DataPaths,
    source_root: &Path,
    bundled_trust: &ArtifactTrustStore,
) -> Result<Vec<CertificationInstallation>, String> {
    let source_certifications = source_root.join("certifications");
    match std::fs::symlink_metadata(&source_certifications) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "bundled certifications path is not a real directory: {}",
                source_certifications.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "inspect bundled certifications {}: {error}",
                source_certifications.display()
            ));
        }
    }

    let mut entries = std::fs::read_dir(&source_certifications)
        .map_err(|error| format!("list bundled certifications: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read bundled certification entry: {error}"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    if entries.is_empty() {
        return Err("bundled certifications directory is empty".to_owned());
    }

    let mut certification_ids = BTreeSet::new();
    let mut plan = Vec::with_capacity(entries.len());
    for entry in entries {
        let source = entry.path();
        let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
            format!(
                "inspect bundled certification {}: {error}",
                source.display()
            )
        })?;
        if !metadata.file_type().is_file()
            || source
                .extension()
                .is_none_or(|extension| extension != "json")
        {
            return Err(format!(
                "unexpected bundled certification entry (only regular .json files are allowed): {}",
                source.display()
            ));
        }
        let destination = paths.certifications.join(entry.file_name());
        let bytes = read_certification(&source)?;
        let bundled: HostCertificationV2 = serde_json::from_slice(&bytes).map_err(|error| {
            format!("decode bundled certification {}: {error}", source.display())
        })?;
        let payload = certification_payload(&bundled)?;
        bundled_trust
            .verify_detached(&bundled.signer_key_id, &bundled.signature_ed25519, &payload)
            .map_err(|error| {
                format!(
                    "verify bundled certification signature {}: {error}",
                    source.display()
                )
            })?;
        if !certification_ids.insert(bundled.certification_id) {
            return Err(format!(
                "duplicate bundled certification id: {}",
                bundled.certification_id
            ));
        }
        if path_entry_exists(&destination)? {
            let existing_bytes = read_certification(&destination)?;
            if existing_bytes == bytes {
                continue;
            }
            let existing: HostCertificationV2 =
                serde_json::from_slice(&existing_bytes).map_err(|error| {
                    format!(
                        "decode installed certification {}: {error}",
                        destination.display()
                    )
                })?;
            if !same_certification_identity(&existing, &bundled) {
                return Err(format!(
                    "bundled certification identity conflicts with {}",
                    destination.display()
                ));
            }
            if bundled.issued_at <= existing.issued_at {
                continue;
            }
            if bundled.expires_at <= existing.expires_at {
                return Err(format!(
                    "newer bundled certification does not extend expiry for {}",
                    destination.display()
                ));
            }
        }
        plan.push(CertificationInstallation { destination, bytes });
    }
    Ok(plan)
}

fn path_entry_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "inspect release material {}: {error}",
            path.display()
        )),
    }
}

fn same_certification_identity(
    current: &HostCertificationV2,
    replacement: &HostCertificationV2,
) -> bool {
    current.schema_version == replacement.schema_version
        && current.platform == replacement.platform
        && current.architecture == replacement.architecture
        && current.guest_kind == replacement.guest_kind
        && current.capability_fingerprint == replacement.capability_fingerprint
        && current.guest_bundle_digest == replacement.guest_bundle_digest
        && current.host_bundle_digest == replacement.host_bundle_digest
        && current.device_profile == replacement.device_profile
        && current.control_protocol_version == replacement.control_protocol_version
        && current.frame_protocol_version == replacement.frame_protocol_version
        && current.signer_key_id == replacement.signer_key_id
}

#[derive(Debug, Clone)]
pub struct CapabilityDiscovery {
    paths: DataPaths,
    default_crosvm: PathBuf,
    default_adb: PathBuf,
    verified_bundles: Arc<ParkingRwLock<Vec<VerifiedBundleCacheEntry>>>,
}

#[derive(Debug, Clone)]
struct VerifiedBundleCacheEntry {
    selection: ArtifactSelectionV2,
    trust_sha256: String,
    bundles: ResolvedBundleSetV2,
}

#[derive(Debug, Clone)]
pub struct CapabilityDiscoveryResultV2 {
    pub capabilities: HostCapabilitiesV2,
    pub bundles: Option<ResolvedBundleSetV2>,
    pub crosvm: PathBuf,
    pub adb: PathBuf,
    pub frame_probe: Option<FrameInteropProbeV2>,
    pub frame_tool: Option<PathBuf>,
    pub adb_bridge: Option<PathBuf>,
}

impl CapabilityDiscovery {
    pub fn new(paths: DataPaths, default_crosvm: PathBuf, default_adb: PathBuf) -> Self {
        Self {
            paths,
            default_crosvm,
            default_adb,
            verified_bundles: Arc::new(ParkingRwLock::new(Vec::new())),
        }
    }

    pub fn discover_defaults(paths: DataPaths, repo_root: Option<&Path>) -> Self {
        let crosvm = std::env::var_os("HD_CROSVM_PATH")
            .map(PathBuf::from)
            .or_else(|| sibling_executable("crosvm"))
            .or_else(|| {
                repo_root.map(|root| {
                    root.join("out")
                        .join("dist")
                        .join("windows")
                        .join("bin")
                        .join(executable_name("crosvm"))
                })
            })
            .unwrap_or_else(|| PathBuf::from(executable_name("crosvm")));
        let adb = std::env::var_os("HD_ADB_PATH")
            .map(PathBuf::from)
            .or_else(|| sibling_executable("adb"))
            .unwrap_or_else(|| PathBuf::from(executable_name("adb")));
        Self::new(paths, crosvm, adb)
    }

    #[allow(clippy::too_many_lines)]
    pub async fn discover(&self, spec: Option<&InstanceSpecV2>) -> CapabilityDiscoveryResultV2 {
        self.discover_inner(spec, false).await
    }

    /// Reuses only bundle sets that were fully signature- and content-verified earlier in this
    /// process. Dynamic resource, executable, certification and device probes still run on every
    /// request. Lifecycle starts use [`Self::discover`] and therefore always hash the selected
    /// artifacts again before authorizing a VM.
    #[allow(clippy::too_many_lines)]
    pub async fn discover_cached(
        &self,
        spec: Option<&InstanceSpecV2>,
    ) -> CapabilityDiscoveryResultV2 {
        self.discover_inner(spec, true).await
    }

    /// Evaluates only dynamic Host CPU, memory and disk admission for one instance.
    ///
    /// This path intentionally does not load trust material, hash artifact bundles, execute
    /// device probes or produce a release fingerprint. It is an explanatory UI preflight; the
    /// lifecycle start path must continue to call [`Self::discover`] before authorizing a VM.
    pub fn resource_admission(
        &self,
        spec: &InstanceSpecV2,
        active_leases: &[LeaseV2],
    ) -> CapabilityProbeV2 {
        resource_probe(&self.paths, Some(spec), Some(active_leases))
    }

    #[allow(clippy::too_many_lines)]
    async fn discover_inner(
        &self,
        spec: Option<&InstanceSpecV2>,
        allow_verified_bundle_cache: bool,
    ) -> CapabilityDiscoveryResultV2 {
        let fast_artifacts = crate::dev::fast_artifacts_enabled();
        let fast_capabilities = crate::dev::fast_capabilities_enabled();
        let dev_display_copy_fallback = crate::dev::allow_display_copy_fallback_enabled();
        let native_display_direct = crate::dev::native_display_direct_enabled();
        let mut probes = Vec::new();
        probes.push(platform_baseline_probe());
        probes.push(hypervisor_probe());
        let resources = resource_probe(&self.paths, spec, None);
        let defer_android_artifacts =
            spec.is_some() && resources.required && resources.status == CapabilityStatusV2::Blocked;
        probes.push(resources);
        if spec.is_some_and(|instance| instance.guest_kind == GuestKindV2::Microdroid) {
            return self.discover_microdroid(probes, spec);
        }

        let trust_path = self.paths.root.join("trusted-keys-v2.json");
        let trust = ArtifactTrustStore::load(&trust_path);
        let trust_sha256 = std::fs::read(&trust_path)
            .ok()
            .map(|bytes| hex::encode(Sha256::digest(bytes)));
        probes.push(match &trust {
            Ok(_) => supported_probe(
                "artifact.trust",
                true,
                "trusted Ed25519 key set loaded",
                BTreeMap::from([("path".to_owned(), trust_path.display().to_string())]),
            ),
            Err(error) => blocked_probe(
                "artifact.trust",
                true,
                error.to_string(),
                BTreeMap::from([("path".to_owned(), trust_path.display().to_string())]),
            ),
        });

        let mut bundles = None;
        let mut crosvm = self.default_crosvm.clone();
        let mut adb = spec
            .and_then(|instance| instance.adb.executable.clone())
            .unwrap_or_else(|| self.default_adb.clone());
        let mut frame_probe = None;
        let mut frame_tool = None;
        let mut adb_bridge = None;
        let mut adb_bridge_probe = None;
        let mut devices =
            unavailable_devices("no verified host bundle is selected for this capability request");

        if defer_android_artifacts {
            let instance_id = spec.map(|instance| instance.id);
            tracing::info!(
                event = "capability.artifacts.deferred",
                ?instance_id,
                reason = "host_resources_blocked",
                "deferred expensive Android artifact and device probes until resource admission"
            );
            probes.push(blocked_probe(
                "artifact.bundles",
                true,
                "artifact verification is deferred until CPU, memory and disk resource admission succeeds"
                    .to_owned(),
                BTreeMap::from([(
                    "deferred_by".to_owned(),
                    "host.resources".to_owned(),
                )]),
            ));
            probes.push(blocked_probe(
                "display.zero_copy",
                true,
                "zero-copy probing is deferred until resource admission and signed bundle verification succeed"
                    .to_owned(),
                BTreeMap::from([(
                    "deferred_by".to_owned(),
                    "host.resources".to_owned(),
                )]),
            ));
        } else {
            match (spec, trust.as_ref()) {
                (Some(instance), Ok(trust)) => match &instance.artifacts {
                    Some(selection) => {
                        let resolver = ArtifactResolver::new((*trust).clone());
                        match resolve_bundles(
                            resolver,
                            selection.clone(),
                            fast_artifacts,
                            allow_verified_bundle_cache,
                            trust_sha256.as_deref(),
                            &self.verified_bundles,
                        )
                        .await
                        {
                            Ok(bundle_set) => {
                                crosvm = bundle_set
                                    .artifacts
                                    .host_tools
                                    .get("crosvm")
                                    .cloned()
                                    .unwrap_or(crosvm);
                                if let Some(bundle_adb) = bundle_set.artifacts.host_tools.get("adb")
                                {
                                    adb = bundle_adb.clone();
                                }
                                devices = if fast_capabilities {
                                    fast_device_capabilities(
                                        &bundle_set.artifacts.host_tools,
                                        &bundle_set.artifacts.sensor_injector,
                                    )
                                } else {
                                    Box::pin(device_capabilities(
                                        &bundle_set.artifacts.host_tools,
                                        &bundle_set.artifacts.sensor_injector,
                                    ))
                                    .await
                                };
                                frame_tool = bundle_set
                                    .artifacts
                                    .host_tools
                                    .get("frame-producer")
                                    .cloned();
                                probes.push(supported_probe(
                                "artifact.bundles",
                                true,
                                if fast_artifacts {
                                    "dev fast path used manifest-only bundle resolution without file hashing"
                                } else {
                                    "guest and host bundles passed signature, READY and file validation"
                                },
                                BTreeMap::from([
                                    (
                                        "guest_digest".to_owned(),
                                        bundle_set.guest_manifest.digest.clone(),
                                    ),
                                    (
                                        "host_digest".to_owned(),
                                        bundle_set.host_manifest.digest.clone(),
                                    ),
                                ]),
                            ));
                                if native_display_direct {
                                    probes.push(supported_probe(
                                    "display.zero_copy",
                                    true,
                                    if cfg!(target_os = "macos") {
                                        "macOS CoreAnimation remote layer is rendered directly by crosvm/gfxstream"
                                    } else {
                                        "Windows native child HWND is rendered directly by crosvm/gfxstream"
                                    },
                                    BTreeMap::from([(
                                        "mode".to_owned(),
                                        if cfg!(target_os = "macos") {
                                            "mac_ca_context"
                                        } else {
                                            "native_child_hwnd"
                                        }
                                        .to_owned(),
                                    )]),
                                ));
                                } else if dev_display_copy_fallback {
                                    probes.push(supported_probe(
                                    "display.zero_copy",
                                    true,
                                    "dev display copy fallback skips strict zero-copy probing for local iteration",
                                    BTreeMap::from([(
                                        "mode".to_owned(),
                                        "dev_display_copy_fallback".to_owned(),
                                    )]),
                                ));
                                } else if fast_capabilities {
                                    probes.push(fast_supported_probe(
                                    "display.zero_copy",
                                    true,
                                    "dev fast capability mode trusts the selected host bundle frame role without running the zero-copy probe",
                                    BTreeMap::from([(
                                        "mode".to_owned(),
                                        "dev_fast_capabilities".to_owned(),
                                    )]),
                                ));
                                } else if let Some(frame_tool) = &frame_tool {
                                    match run_json_probe::<FrameInteropProbeV2>(
                                        frame_tool,
                                        &["--probe-v2", "--json"],
                                    )
                                    .await
                                    {
                                        Ok(probe) if probe.supported() => {
                                            probes.push(supported_probe(
                                                "display.zero_copy",
                                                true,
                                                probe.detail.clone(),
                                                probe.properties.clone(),
                                            ));
                                            frame_probe = Some(probe);
                                        }
                                        Ok(probe) => {
                                            probes.push(blocked_probe(
                                                "display.zero_copy",
                                                true,
                                                probe.detail.clone(),
                                                probe.properties.clone(),
                                            ));
                                            frame_probe = Some(probe);
                                        }
                                        Err(error) => probes.push(blocked_probe(
                                            "display.zero_copy",
                                            true,
                                            error,
                                            BTreeMap::from([(
                                                "executable".to_owned(),
                                                frame_tool.display().to_string(),
                                            )]),
                                        )),
                                    }
                                } else {
                                    probes.push(blocked_probe(
                                        "display.zero_copy",
                                        true,
                                        "verified host bundle has no frame-producer role"
                                            .to_owned(),
                                        BTreeMap::new(),
                                    ));
                                }
                                let adb_required = true;
                                adb_bridge =
                                    bundle_set.artifacts.host_tools.get("adb-bridge").cloned();
                                let status = if fast_capabilities {
                                    fast_formal_tool_status(
                                        adb_required,
                                        &bundle_set.artifacts.host_tools,
                                        "adb-bridge",
                                    )
                                } else {
                                    formal_tool_status(
                                        adb_required,
                                        &bundle_set.artifacts.host_tools,
                                        "adb-bridge",
                                        "adb-bridge",
                                        &[
                                            "loopback-tcp-v2",
                                            "vsock-guest-v2",
                                            "ready-marker-v2",
                                            "lifecycle-v2",
                                        ],
                                    )
                                    .await
                                };
                                adb_bridge_probe = Some(formal_component_capability_probe(
                                    "adb.bridge",
                                    adb_required,
                                    &status,
                                ));
                                bundles = Some(bundle_set);
                            }
                            Err(error) => probes.push(blocked_probe(
                                "artifact.bundles",
                                true,
                                error,
                                BTreeMap::new(),
                            )),
                        }
                    }
                    None => probes.push(blocked_probe(
                        "artifact.bundles",
                        true,
                        "instance has no signed artifact selection".to_owned(),
                        BTreeMap::new(),
                    )),
                },
                (Some(_), Err(_)) => probes.push(blocked_probe(
                    "artifact.bundles",
                    true,
                    "artifact trust store is unavailable".to_owned(),
                    BTreeMap::new(),
                )),
                (None, _) => {
                    probes.push(blocked_probe(
                        "artifact.bundles",
                        false,
                        "select an instance to validate its signed bundles".to_owned(),
                        BTreeMap::new(),
                    ));
                    probes.push(blocked_probe(
                        "display.zero_copy",
                        false,
                        "zero-copy is probed from the selected signed host bundle".to_owned(),
                        BTreeMap::new(),
                    ));
                }
            }
        }

        probes.push(
            tool_probe(
                "tool.crosvm",
                &crosvm,
                true,
                &["version"],
                fast_capabilities,
            )
            .await,
        );
        let adb_required = spec.is_some();
        probes.push(
            tool_probe(
                "tool.adb",
                &adb,
                adb_required,
                &["version"],
                fast_capabilities,
            )
            .await,
        );
        probes.push(adb_bridge_probe.unwrap_or_else(|| {
            blocked_probe(
                "adb.bridge",
                adb_required,
                if adb_required {
                    "no verified adb-bridge component is available".to_owned()
                } else {
                    "select an instance to validate its ADB bridge".to_owned()
                },
                BTreeMap::new(),
            )
        }));
        probes.push(match spec {
            Some(instance) if matches!(instance.adb.mode, hd_core::AdbModeV2::Loopback) => {
                supported_probe(
                    "guest.readiness",
                    true,
                    if dev_display_copy_fallback {
                        "V2 readiness uses authenticated loopback ADB after dev display fallback"
                            .to_owned()
                    } else {
                        "V2 readiness uses authenticated loopback ADB after the strict frame handshake"
                            .to_owned()
                    },
                    BTreeMap::new(),
                )
            }
            Some(_) => blocked_probe(
                "guest.readiness",
                true,
                "V2 requires loopback ADB for per-run boot-completed and package-manager readiness"
                    .to_owned(),
                BTreeMap::new(),
            ),
            None => blocked_probe(
                "guest.readiness",
                false,
                "select an instance to validate its readiness channel".to_owned(),
                BTreeMap::new(),
            ),
        });
        probes.push(device_probe(&devices, spec));

        let evidence_fingerprint = release_fingerprint(&probes, &devices, false);
        let signed_development_artifacts = bundles.as_ref().is_some_and(|bundle_set| {
            bundle_set
                .guest_manifest
                .capabilities
                .iter()
                .any(|capability| capability == "android-data-unencrypted-development-v1")
        });
        let development_bypass = spec.is_some() && (fast_artifacts || signed_development_artifacts);
        let verified = !development_bypass
            && !fast_capabilities
            && !dev_display_copy_fallback
            && probes.iter().all(|probe| {
                !probe.required || matches!(probe.status, CapabilityStatusV2::Supported)
            });
        let certified = if development_bypass {
            probes.push(blocked_probe(
                "release.certification",
                false,
                if signed_development_artifacts {
                    "release certification was not evaluated because the signed Guest bundle declares the development-only unencrypted Android data profile"
                } else {
                    "release certification was not evaluated because development artifact bypass is active"
                },
                BTreeMap::from([(
                    "evidence_fingerprint".to_owned(),
                    evidence_fingerprint.clone(),
                )])
                .into_iter()
                .chain([(
                    "mode".to_owned(),
                    if signed_development_artifacts {
                        "signed_development_data_profile"
                    } else {
                        "development_bypass"
                    }
                    .to_owned(),
                )])
                .collect(),
            ));
            false
        } else {
            let certification = spec
                .and_then(|instance| instance.artifacts.as_ref())
                .ok_or_else(|| "instance has no signed bundle selection".to_owned())
                .and_then(|selection| {
                    certification_matches(
                        &self.paths,
                        spec,
                        &CertificationIdentity {
                            guest_kind: GuestKindV2::Android,
                            guest_digest: &selection.guest_bundle_digest,
                            host_digest: &selection.host_bundle_digest,
                            device_profile: "hd-phone-android15-v2",
                        },
                        &evidence_fingerprint,
                        trust.as_ref().ok(),
                    )
                });
            let certified = certification.is_ok();
            probes.push(match certification {
                Ok(certification) => supported_probe(
                    "release.certification",
                    true,
                    "signed real-guest, zero-copy and device-profile evidence matches this platform and bundle",
                    BTreeMap::from([
                        ("evidence_fingerprint".to_owned(), evidence_fingerprint.clone()),
                        (
                            "certification_id".to_owned(),
                            certification.certification_id.to_string(),
                        ),
                        ("issued_at".to_owned(), certification.issued_at.to_string()),
                        ("expires_at".to_owned(), certification.expires_at.to_string()),
                        (
                            "evidence_count".to_owned(),
                            certification.evidence_sha256.len().to_string(),
                        ),
                    ]),
                ),
                Err(error) => blocked_probe(
                    "release.certification",
                    true,
                    error,
                    BTreeMap::from([(
                        "evidence_fingerprint".to_owned(),
                        evidence_fingerprint.clone(),
                    )]),
                ),
            });
            certified
        };
        let fingerprint = capability_fingerprint(&probes, &devices);
        let capabilities = HostCapabilitiesV2 {
            schema_version: 2,
            generated_at: OffsetDateTime::now_utc(),
            platform: platform_name().to_owned(),
            architecture: architecture_name().to_owned(),
            fingerprint,
            verified,
            certified,
            development_bypass,
            probes,
            devices,
        };
        CapabilityDiscoveryResultV2 {
            capabilities,
            bundles,
            crosvm,
            adb,
            frame_probe,
            frame_tool,
            adb_bridge,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn discover_microdroid(
        &self,
        mut probes: Vec<CapabilityProbeV2>,
        spec: Option<&InstanceSpecV2>,
    ) -> CapabilityDiscoveryResultV2 {
        let runtime = resolve_microdroid_runtime_paths();
        let apex_tree = runtime.product_root.join("apex_dir");
        let vm = runtime.vm();
        let virtmgr = runtime.virtmgr();
        let crosvm = runtime.crosvm();
        let required = [
            ("microdroid.host.vm", vm.clone()),
            ("microdroid.host.virtmgr", virtmgr),
            ("microdroid.host.crosvm", crosvm.clone()),
            (
                "microdroid.guest.kernel",
                apex_tree.join("apex/com.android.virt/etc/fs/microdroid_kernel"),
            ),
            (
                "microdroid.guest.super",
                apex_tree.join("apex/com.android.virt/etc/fs/microdroid_super.img"),
            ),
            (
                "microdroid.guest.config",
                apex_tree.join("apex/com.android.virt/etc/microdroid.json"),
            ),
            ("microdroid.host.binder_rpc", runtime.binder_rpc()),
        ];
        for (id, path) in required {
            let properties = BTreeMap::from([("path".to_owned(), path.display().to_string())]);
            probes.push(if path.is_file() {
                supported_probe(
                    id,
                    true,
                    "required Microdroid artifact is present",
                    properties,
                )
            } else {
                blocked_probe(
                    id,
                    true,
                    "required Microdroid artifact is missing",
                    properties,
                )
            });
        }
        let microdroid_config = spec.and_then(|instance| instance.microdroid.as_ref());
        let selected_extra_apks = microdroid_config.map_or(0, |config| config.extra_apks.len());
        let declared_extra_apks =
            microdroid_config.and_then(|config| config.payload_extra_apk_count);
        let extra_apks_complete =
            declared_extra_apks.is_none_or(|declared| usize::from(declared) == selected_extra_apks);
        let profile_properties = BTreeMap::from([
            ("guest_kind".to_owned(), "microdroid".to_owned()),
            ("protected".to_owned(), "false".to_owned()),
            (
                "debug".to_owned(),
                microdroid_config
                    .map_or("full", |config| match config.debug_level {
                        hd_core::MicrodroidDebugLevelV2::None => "none",
                        hd_core::MicrodroidDebugLevelV2::Full => "full",
                    })
                    .to_owned(),
            ),
            (
                "extra_apk_count".to_owned(),
                selected_extra_apks.to_string(),
            ),
            (
                "extra_apk_declared_count".to_owned(),
                declared_extra_apks.map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
            ),
            (
                "extra_apk_selection_complete".to_owned(),
                extra_apks_complete.to_string(),
            ),
            (
                "extra_apk_max".to_owned(),
                hd_core::MAX_MICRODROID_EXTRA_APKS.to_string(),
            ),
            (
                "extra_apk_descriptor_override".to_owned(),
                "true".to_owned(),
            ),
            (
                "console_challenge".to_owned(),
                if microdroid_config.is_some_and(|config| {
                    config.debug_level == hd_core::MicrodroidDebugLevelV2::Full
                }) {
                    if cfg!(windows) {
                        "unavailable"
                    } else {
                        "typed-nonce-v1"
                    }
                } else {
                    "disabled"
                }
                .to_owned(),
            ),
        ]);
        probes.push(if !extra_apks_complete {
            blocked_probe(
                "microdroid.profile",
                true,
                format!(
                    "Microdroid Payload declares {} extra APKs but the instance selected {selected_extra_apks}",
                    declared_extra_apks.expect("incomplete selection has a declared count")
                ),
                profile_properties,
            )
        } else if microdroid_platform_supported() {
            supported_probe(
                "microdroid.profile",
                true,
                format!(
                    "{} {} unprotected headless Microdroid profile",
                    platform_name(),
                    architecture_name()
                ),
                profile_properties,
            )
        } else {
            blocked_probe(
                "microdroid.profile",
                true,
                "Microdroid is supported on macOS arm64 and Windows x86_64",
                profile_properties,
            )
        });
        let adb = spec
            .and_then(|instance| instance.adb.executable.clone())
            .unwrap_or_else(|| self.default_adb.clone());
        if spec.is_some_and(|instance| matches!(instance.adb.mode, hd_core::AdbModeV2::Loopback)) {
            let properties = BTreeMap::from([("path".to_owned(), adb.display().to_string())]);
            probes.push(if adb.is_file() {
                supported_probe(
                    "microdroid.host.adb",
                    true,
                    "instance-selected ADB executable is present",
                    properties,
                )
            } else {
                blocked_probe(
                    "microdroid.host.adb",
                    true,
                    "instance-selected ADB executable is missing",
                    properties,
                )
            });
        }
        let devices = unavailable_devices("Android device simulation is not applicable");
        let development_bypass = explicit_microdroid_development_bypass();
        let trust_path = self.paths.root.join("trusted-keys-v2.json");
        let trust = ArtifactTrustStore::load(&trust_path);
        probes.push(match &trust {
            Ok(_) => supported_probe(
                "artifact.trust",
                true,
                "trusted Ed25519 key set loaded",
                BTreeMap::from([("path".to_owned(), trust_path.display().to_string())]),
            ),
            Err(error) if development_bypass => blocked_probe(
                "artifact.trust",
                false,
                error.to_string(),
                BTreeMap::from([("path".to_owned(), trust_path.display().to_string())]),
            ),
            Err(error) => blocked_probe(
                "artifact.trust",
                true,
                error.to_string(),
                BTreeMap::from([("path".to_owned(), trust_path.display().to_string())]),
            ),
        });
        let identity_path = runtime.identity();
        let identity = load_microdroid_runtime_identity(&identity_path, &runtime.product_root);
        probes.push(match &identity {
            Ok(identity) => supported_probe(
                "microdroid.artifact.identity",
                true,
                "packaged Microdroid runtime identity is present",
                BTreeMap::from([
                    ("path".to_owned(), identity_path.display().to_string()),
                    ("profile".to_owned(), identity.profile.clone()),
                    ("guest_digest".to_owned(), identity.guest_digest.clone()),
                    ("host_digest".to_owned(), identity.host_digest.clone()),
                ]),
            ),
            Err(error) if development_bypass => blocked_probe(
                "microdroid.artifact.identity",
                false,
                error.clone(),
                BTreeMap::from([("path".to_owned(), identity_path.display().to_string())]),
            ),
            Err(error) => blocked_probe(
                "microdroid.artifact.identity",
                true,
                error.clone(),
                BTreeMap::from([("path".to_owned(), identity_path.display().to_string())]),
            ),
        });
        probes.push(if development_bypass {
            supported_probe(
                "microdroid.development_bypass",
                false,
                "explicit local Microdroid development bypass is enabled",
                BTreeMap::from([(
                    "environment".to_owned(),
                    MICRODROID_DEV_BYPASS_ENV.to_owned(),
                )]),
            )
        } else {
            blocked_probe(
                "microdroid.development_bypass",
                false,
                "development bypass is disabled",
                BTreeMap::new(),
            )
        });
        let evidence_fingerprint = release_fingerprint(&probes, &devices, true);
        let certified = if development_bypass {
            probes.push(blocked_probe(
                "release.certification",
                false,
                "release certification was not evaluated because the explicit Microdroid development bypass is active",
                BTreeMap::from([
                    ("evidence_fingerprint".to_owned(), evidence_fingerprint.clone()),
                    ("mode".to_owned(), "development_bypass".to_owned()),
                ]),
            ));
            false
        } else {
            let certification = identity
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|identity| {
                    certification_matches(
                        &self.paths,
                        spec,
                        &CertificationIdentity {
                            guest_kind: GuestKindV2::Microdroid,
                            guest_digest: &identity.guest_digest,
                            host_digest: &identity.host_digest,
                            device_profile: &identity.profile,
                        },
                        &evidence_fingerprint,
                        trust.as_ref().ok(),
                    )
                });
            let certified = certification.is_ok();
            probes.push(match certification {
                Ok(certification) => supported_probe(
                    "release.certification",
                    true,
                    "signed Microdroid real-guest, multi-instance and payload evidence matches this packaged runtime",
                    BTreeMap::from([
                        ("evidence_fingerprint".to_owned(), evidence_fingerprint.clone()),
                        (
                            "certification_id".to_owned(),
                            certification.certification_id.to_string(),
                        ),
                        ("issued_at".to_owned(), certification.issued_at.to_string()),
                        ("expires_at".to_owned(), certification.expires_at.to_string()),
                        (
                            "evidence_count".to_owned(),
                            certification.evidence_sha256.len().to_string(),
                        ),
                    ]),
                ),
                Err(error) => blocked_probe(
                    "release.certification",
                    true,
                    error,
                    BTreeMap::from([(
                        "evidence_fingerprint".to_owned(),
                        evidence_fingerprint.clone(),
                    )]),
                ),
            });
            certified
        };
        let verified = !development_bypass
            && identity.is_ok()
            && trust.is_ok()
            && probes.iter().all(|probe| {
                !probe.required || matches!(probe.status, CapabilityStatusV2::Supported)
            });
        let capabilities = HostCapabilitiesV2 {
            schema_version: 2,
            generated_at: OffsetDateTime::now_utc(),
            platform: platform_name().to_owned(),
            architecture: architecture_name().to_owned(),
            fingerprint: capability_fingerprint(&probes, &devices),
            verified,
            certified,
            development_bypass,
            probes,
            devices,
        };
        CapabilityDiscoveryResultV2 {
            capabilities,
            bundles: None,
            crosvm,
            adb,
            frame_probe: None,
            frame_tool: None,
            adb_bridge: None,
        }
    }
}

fn load_microdroid_runtime_identity(
    path: &Path,
    product_root: &Path,
) -> Result<MicrodroidRuntimeIdentityV2, String> {
    const LIMIT: u64 = 64 * 1024;
    let bytes = hd_platform::read_regular_nofollow_limited(path, LIMIT).map_err(|error| {
        format!(
            "read Microdroid runtime identity {}: {error}",
            path.display()
        )
    })?;
    let identity: MicrodroidRuntimeIdentityV2 =
        serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "decode Microdroid runtime identity {}: {error}",
                path.display()
            )
        })?;
    if identity.schema_version != MICRODROID_IDENTITY_VERSION
        || identity.profile != microdroid_profile()
        || !valid_digest(&identity.guest_digest)
        || !valid_digest(&identity.host_digest)
    {
        return Err("Microdroid runtime identity is invalid or unsupported".to_owned());
    }
    let directory = path
        .parent()
        .ok_or_else(|| "Microdroid runtime identity has no parent directory".to_owned())?;
    let guest_manifest = hash_regular_file(&directory.join("guest-files-v2.sha256"))?;
    let host_manifest = hash_regular_file(&directory.join("host-files-v2.sha256"))?;
    if guest_manifest != identity.guest_digest || host_manifest != identity.host_digest {
        return Err(
            "Microdroid runtime identity does not match its sealed file manifests".to_owned(),
        );
    }
    verify_microdroid_guest_files(product_root, &directory.join("guest-files-v2.sha256"))?;
    Ok(identity)
}

fn verify_microdroid_guest_files(product_root: &Path, manifest: &Path) -> Result<(), String> {
    const MANIFEST_LIMIT: u64 = 1024 * 1024;
    const MAX_MANIFEST_FILES: usize = 128;
    let bytes =
        hd_platform::read_regular_nofollow_limited(manifest, MANIFEST_LIMIT).map_err(|error| {
            format!(
                "read Microdroid guest manifest {}: {error}",
                manifest.display()
            )
        })?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("decode Microdroid guest manifest: {error}"))?;
    let mut expected = BTreeMap::new();
    for line in text.lines() {
        let (digest, path) = line
            .split_once("  ")
            .ok_or_else(|| "Microdroid guest manifest line is malformed".to_owned())?;
        if !valid_digest(digest) {
            return Err("Microdroid guest manifest contains an invalid digest".to_owned());
        }
        let relative = strict_manifest_path(path)?;
        if expected.insert(relative, digest.to_owned()).is_some() {
            return Err("Microdroid guest manifest contains a duplicate path".to_owned());
        }
        if expected.len() > MAX_MANIFEST_FILES {
            return Err("Microdroid guest manifest exceeds the file-count limit".to_owned());
        }
    }
    if expected.is_empty() {
        return Err("Microdroid guest manifest is empty".to_owned());
    }
    let mut actual = BTreeSet::new();
    collect_microdroid_guest_files(product_root, product_root, 0, &mut actual)?;
    let expected_paths = expected.keys().cloned().collect::<BTreeSet<_>>();
    if actual != expected_paths {
        return Err(format!(
            "Microdroid runtime closure differs from its sealed manifest: expected {} files, found {}",
            expected_paths.len(),
            actual.len()
        ));
    }
    for (relative, expected_digest) in expected {
        let path = product_root.join(&relative);
        let actual_digest = hash_regular_file(&path)?;
        if actual_digest != expected_digest {
            return Err(format!(
                "Microdroid runtime file digest mismatch: {}",
                relative.display()
            ));
        }
    }
    Ok(())
}

fn strict_manifest_path(value: &str) -> Result<PathBuf, String> {
    const MAX_PATH_BYTES: usize = 4096;
    let relative = value
        .strip_prefix("./")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "Microdroid guest manifest path must start with ./".to_owned())?;
    if relative.len() > MAX_PATH_BYTES || relative.contains('\\') {
        return Err("Microdroid guest manifest path is outside the path contract".to_owned());
    }
    let path = PathBuf::from(relative);
    if !path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("Microdroid guest manifest path contains unsafe components".to_owned());
    }
    Ok(path)
}

fn collect_microdroid_guest_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    const MAX_DEPTH: usize = 16;
    const MAX_FILES: usize = 128;
    if depth > MAX_DEPTH {
        return Err("Microdroid runtime closure exceeds the directory-depth limit".to_owned());
    }
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| format!("inspect {}: {error}", directory.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "Microdroid runtime closure contains a non-directory container: {}",
            directory.display()
        ));
    }
    for entry in std::fs::read_dir(directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
    {
        let entry =
            entry.map_err(|error| format!("read entry in {}: {error}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "Microdroid runtime closure contains a symbolic link: {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            collect_microdroid_guest_files(root, &path, depth + 1, files)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("relativize {}: {error}", path.display()))?
                .to_path_buf();
            files.insert(relative);
            if files.len() > MAX_FILES {
                return Err("Microdroid runtime closure exceeds the file-count limit".to_owned());
            }
        } else {
            return Err(format!(
                "Microdroid runtime closure contains a special file: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn hash_regular_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn explicit_microdroid_development_bypass() -> bool {
    std::env::var(MICRODROID_DEV_BYPASS_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

async fn resolve_bundles(
    resolver: ArtifactResolver,
    selection: ArtifactSelectionV2,
    fast_artifacts: bool,
    allow_verified_cache: bool,
    trust_sha256: Option<&str>,
    verified_bundles: &ParkingRwLock<Vec<VerifiedBundleCacheEntry>>,
) -> Result<ResolvedBundleSetV2, String> {
    if allow_verified_cache
        && !fast_artifacts
        && let Some(trust_sha256) = trust_sha256
        && let Some(entry) = verified_bundles.read().iter().find(|entry| {
            entry.selection == selection && entry.trust_sha256.as_str() == trust_sha256
        })
    {
        return Ok(entry.bundles.clone());
    }

    let cache_key = selection.clone();
    let bundle_set = tokio::task::spawn_blocking(move || {
        if fast_artifacts {
            resolver.resolve_fast_dev(&selection)
        } else {
            resolver.resolve(&selection)
        }
    })
    .await
    .map_err(|error| format!("artifact resolver task failed: {error}"))?
    .map_err(|error| error.to_string())?;
    if !fast_artifacts && let Some(trust_sha256) = trust_sha256 {
        let mut cache = verified_bundles.write();
        if let Some(entry) = cache.iter_mut().find(|entry| entry.selection == cache_key) {
            trust_sha256.clone_into(&mut entry.trust_sha256);
            entry.bundles = bundle_set.clone();
        } else {
            cache.push(VerifiedBundleCacheEntry {
                selection: cache_key,
                trust_sha256: trust_sha256.to_owned(),
                bundles: bundle_set.clone(),
            });
        }
    }
    Ok(bundle_set)
}

async fn tool_probe(
    id: &str,
    path: &Path,
    required: bool,
    arguments: &[&str],
    fast_capabilities: bool,
) -> CapabilityProbeV2 {
    let properties = BTreeMap::from([("path".to_owned(), path.display().to_string())]);
    if !path.is_file() && path.components().count() > 1 {
        return blocked_probe(id, required, "executable is missing".to_owned(), properties);
    }
    if fast_capabilities {
        return fast_supported_probe(
            id,
            required,
            "dev fast capability mode skipped executable version probe",
            properties,
        );
    }
    match run_tool(path, arguments).await {
        Ok(version) => supported_probe(id, required, version, properties),
        Err(error) => blocked_probe(id, required, error, properties),
    }
}

async fn run_tool(path: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = run_probe_process(path, arguments).await?;
    if !output.status.success() {
        return Err(format!(
            "tool probe failed with code {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(if version.is_empty() {
        "executable probe succeeded".to_owned()
    } else {
        version
    })
}

async fn run_json_probe<T: serde::de::DeserializeOwned>(
    path: &Path,
    arguments: &[&str],
) -> Result<T, String> {
    let output = run_probe_process(path, arguments).await?;
    if !output.status.success() {
        return Err(format!(
            "JSON probe failed with code {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| format!("decode JSON probe: {error}"))
}

struct ProbeOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_probe_process(path: &Path, arguments: &[&str]) -> Result<ProbeOutput, String> {
    let mut command = Command::new(path);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_managed_command(command.as_std_mut())
        .map_err(|error| format!("configure {} probe: {error}", path.display()))?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {}: {error}", path.display()))?;
    let pid = child
        .id()
        .ok_or_else(|| format!("{} probe has no process id", path.display()))?;
    let containment = match contain_process(pid) {
        Ok(containment) => containment,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(format!("contain {} probe: {error}", path.display()));
        }
    };
    if let Err(error) = resume_managed_process(pid) {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(format!("resume {} probe: {error}", path.display()));
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "probe stdout pipe was not created".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "probe stderr pipe was not created".to_owned())?;
    let exchange = async move {
        tracing::debug!(
            containment_pid = containment.process_id(),
            "probe process containment established"
        );
        let (stdout, stderr, status) = tokio::try_join!(
            read_probe_stream(stdout),
            read_probe_stream(stderr),
            child.wait()
        )?;
        Ok::<_, std::io::Error>(ProbeOutput {
            status,
            stdout,
            stderr,
        })
    };
    tokio::time::timeout(PROBE_TIMEOUT, exchange)
        .await
        .map_err(|_| format!("{} probe timed out", path.display()))?
        .map_err(|error| format!("{} probe I/O failed: {error}", path.display()))
}

async fn read_probe_stream<R>(reader: R) -> Result<Vec<u8>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((MAX_TOOL_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_TOOL_OUTPUT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "probe stream exceeded 1 MiB",
        ));
    }
    Ok(bytes)
}

fn platform_baseline_probe() -> CapabilityProbeV2 {
    let probe = native_platform_baseline();
    if probe.supported {
        supported_probe("platform.baseline", true, probe.detail, probe.properties)
    } else {
        blocked_probe("platform.baseline", true, probe.detail, probe.properties)
    }
}

fn hypervisor_probe() -> CapabilityProbeV2 {
    let probe = native_hypervisor_available();
    if probe.supported {
        supported_probe("hypervisor", true, probe.detail, probe.properties)
    } else {
        blocked_probe("hypervisor", true, probe.detail, probe.properties)
    }
}

#[allow(clippy::too_many_lines)]
fn resource_probe(
    paths: &DataPaths,
    spec: Option<&InstanceSpecV2>,
    active_leases: Option<&[LeaseV2]>,
) -> CapabilityProbeV2 {
    const GIB: u64 = 1024 * 1024 * 1024;
    let resources = hd_platform::host_resources();
    let logical_cpus = resources.as_ref().map_or(0, |value| value.logical_cpus);
    let host_cpu_reserve = host_cpu_reserve(logical_cpus);
    let normal_guest_cpu_capacity = guest_cpu_capacity(logical_cpus);
    let match_host_cpu_topology = spec.is_some_and(uses_match_host_cpu_topology);
    let guest_cpu_capacity = if match_host_cpu_topology {
        logical_cpus
    } else {
        normal_guest_cpu_capacity
    };
    let total_memory_bytes = resources
        .as_ref()
        .map_or(0, |value| value.total_memory_bytes);
    let available_memory_bytes = resources
        .as_ref()
        .map_or(0, |value| value.available_memory_bytes);
    let requested_cpus = spec.map_or(2, |instance| requested_cpu_count(instance, logical_cpus));
    let requested_memory_bytes = spec.map_or(2 * GIB, |instance| {
        u64::from(instance.memory_mib).saturating_mul(1024 * 1024)
    });
    let host_memory_reserve = (total_memory_bytes / 10).max(GIB);
    let required_memory_bytes = requested_memory_bytes.saturating_add(host_memory_reserve);
    let memory_lease_capacity = total_memory_bytes.saturating_sub(host_memory_reserve);
    let (reserved_cpu_count, reserved_memory_bytes, lease_error) = match active_leases {
        Some(leases) => match (
            leased_quantity(leases, LeaseKindV2::CpuCapacity),
            leased_quantity(leases, LeaseKindV2::MemoryBytes),
        ) {
            (Ok(cpu), Ok(memory)) => (cpu, memory, None),
            (Err(error), _) | (_, Err(error)) => (0, 0, Some(error.to_string())),
        },
        None => (0, 0, None),
    };
    let lease_cpu_capacity = u64::try_from(if match_host_cpu_topology {
        logical_cpus
    } else {
        normal_guest_cpu_capacity
    })
    .unwrap_or(0);
    let available_lease_cpus = if match_host_cpu_topology && reserved_cpu_count != 0 {
        0
    } else {
        lease_cpu_capacity.saturating_sub(reserved_cpu_count)
    };
    let available_lease_memory_bytes = memory_lease_capacity.saturating_sub(reserved_memory_bytes);
    let disk_requirement = runtime_disk_requirement(paths, spec);
    let required_disk_bytes = disk_requirement.required_free_bytes;
    let available_bytes = fs2::available_space(&paths.root).unwrap_or(0);
    let mut properties = BTreeMap::from([
        ("logical_cpus".to_owned(), logical_cpus.to_string()),
        ("host_cpu_reserve".to_owned(), host_cpu_reserve.to_string()),
        (
            "guest_cpu_capacity".to_owned(),
            guest_cpu_capacity.to_string(),
        ),
        (
            "normal_guest_cpu_capacity".to_owned(),
            normal_guest_cpu_capacity.to_string(),
        ),
        (
            "cpu_topology".to_owned(),
            if match_host_cpu_topology {
                "match_host"
            } else {
                "fixed_count"
            }
            .to_owned(),
        ),
        (
            "exclusive_cpu_lease".to_owned(),
            match_host_cpu_topology.to_string(),
        ),
        ("requested_cpus".to_owned(), requested_cpus.to_string()),
        ("reserved_cpus".to_owned(), reserved_cpu_count.to_string()),
        (
            "available_lease_cpus".to_owned(),
            available_lease_cpus.to_string(),
        ),
        (
            "total_memory_bytes".to_owned(),
            total_memory_bytes.to_string(),
        ),
        (
            "available_memory_bytes".to_owned(),
            available_memory_bytes.to_string(),
        ),
        (
            "requested_memory_bytes".to_owned(),
            requested_memory_bytes.to_string(),
        ),
        (
            "host_memory_reserve_bytes".to_owned(),
            host_memory_reserve.to_string(),
        ),
        (
            "memory_lease_capacity_bytes".to_owned(),
            memory_lease_capacity.to_string(),
        ),
        (
            "reserved_memory_bytes".to_owned(),
            reserved_memory_bytes.to_string(),
        ),
        (
            "available_lease_memory_bytes".to_owned(),
            available_lease_memory_bytes.to_string(),
        ),
        (
            "active_leases_checked".to_owned(),
            active_leases.is_some().to_string(),
        ),
        (
            "available_disk_bytes".to_owned(),
            available_bytes.to_string(),
        ),
        (
            "required_disk_bytes".to_owned(),
            required_disk_bytes.to_string(),
        ),
        (
            "disk_requirement_mode".to_owned(),
            disk_requirement.mode.as_str().to_owned(),
        ),
    ]);
    if let Ok(resources) = &resources {
        properties.insert(
            "memory_source".to_owned(),
            resources.memory_source.to_owned(),
        );
    }
    if resources.is_ok()
        && lease_error.is_none()
        && requested_cpus > 0
        && guest_cpu_capacity >= requested_cpus
        && u64::try_from(requested_cpus).is_ok_and(|value| value <= available_lease_cpus)
        && available_memory_bytes >= required_memory_bytes
        && requested_memory_bytes <= available_lease_memory_bytes
        && available_bytes >= required_disk_bytes
    {
        supported_probe(
            "host.resources",
            true,
            if match_host_cpu_topology {
                "exclusive match_host CPU topology, memory headroom and disk capacity are available"
                    .to_owned()
            } else {
                "requested CPU, memory headroom and disk capacity are available".to_owned()
            },
            properties,
        )
    } else {
        blocked_probe(
            "host.resources",
            true,
            resources.map_or_else(
                |error| format!("query host resources: {error}"),
                |_| {
                    if let Some(error) = lease_error {
                        return format!("inspect active resource leases: {error}");
                    }
                    if u64::try_from(requested_cpus)
                        .is_ok_and(|value| value > available_lease_cpus)
                    {
                        return if match_host_cpu_topology {
                            format!(
                                "match_host requires exclusive access to all {requested_cpus} logical CPUs, but {reserved_cpu_count} CPUs are reserved by other instances"
                            )
                        } else {
                            format!(
                                "requires {requested_cpus} guest CPUs, but only {available_lease_cpus} remain after active instance reservations"
                            )
                        };
                    }
                    if requested_memory_bytes > available_lease_memory_bytes {
                        return format!(
                            "requires {requested_memory_bytes} memory bytes, but only {available_lease_memory_bytes} remain after active instance reservations"
                        );
                    }
                    if match_host_cpu_topology {
                        format!(
                            "requires an exclusive lease for all {requested_cpus} logical CPUs, {required_memory_bytes} available memory bytes including host reserve, and {required_disk_bytes} free disk bytes"
                        )
                    } else {
                        format!(
                            "requires {requested_cpus} guest CPUs from capacity {guest_cpu_capacity} after reserving {host_cpu_reserve} of {logical_cpus} logical CPUs for the Host, {required_memory_bytes} available memory bytes including host reserve, and {required_disk_bytes} free disk bytes"
                        )
                    }
                },
            ),
            properties,
        )
    }
}

fn device_probe(
    devices: &DeviceCapabilitiesV2,
    spec: Option<&InstanceSpecV2>,
) -> CapabilityProbeV2 {
    let enabled = |id: &str| {
        spec.is_none_or(|spec| match id {
            "bluetooth" => spec.devices.bluetooth,
            "nfc" => spec.devices.nfc,
            "uwb" => spec.devices.uwb,
            "modem" => spec.devices.modem,
            "gnss" => spec.devices.gnss,
            "sensors" => spec.devices.sensors,
            "network" => spec.devices.network,
            "audio" => spec.devices.audio,
            "camera" => spec.devices.camera,
            "power" => spec.devices.power,
            "touchpad" => spec.devices.touchpad,
            _ => true,
        })
    };
    let missing = devices
        .devices
        .iter()
        .filter(|device| enabled(&device.id) && !device.available)
        .map(|device| device.id.clone())
        .collect::<Vec<_>>();
    let mut missing = missing;
    if spec.is_some_and(|spec| {
        matches!(
            spec.host_audio_input,
            hd_core::HostAudioInputV2::DefaultMicrophone
        )
    }) {
        let host_microphone_available = devices.devices.iter().any(|device| {
            device.id == "audio"
                && device.available
                && device
                    .features
                    .iter()
                    .any(|feature| feature == "host_default_microphone")
        });
        if !host_microphone_available {
            missing.push("audio.host_default_microphone".to_owned());
        }
    }
    let required = spec.is_some();
    if missing.is_empty() {
        supported_probe(
            "device.profile",
            required,
            "all enabled phone-profile device backends are available".to_owned(),
            BTreeMap::new(),
        )
    } else {
        blocked_probe(
            "device.profile",
            required,
            format!("missing formal backends: {}", missing.join(", ")),
            BTreeMap::new(),
        )
    }
}

async fn device_capabilities(
    tools: &BTreeMap<String, PathBuf>,
    sensor_injector: &Path,
) -> DeviceCapabilitiesV2 {
    let (simulator, bluetooth, nfc, uwb, modem) = tokio::join!(
        formal_tool_status(
            true,
            tools,
            "hd-device-sim",
            "hd-device-sim",
            &[
                "battery-v2",
                "location-v2",
                "network-condition-v2",
                "sensor-injection-v2",
                "sensor-duration-reset-v1",
                "sensor-pose-v1",
            ],
        ),
        formal_tool_status(
            true,
            tools,
            "rootcanal-adapter",
            "rootcanal-adapter",
            &[
                "guest-serial-v2",
                "hci-v2",
                "peer-control-v2",
                "beacon-peer-control-v2",
                "scripted-beacon-control-v2",
                "hogp-keyboard-control-v2",
                "bounded-hci-capture-v2",
            ],
        ),
        formal_tool_status(
            true,
            tools,
            "casimir-adapter",
            "casimir-adapter",
            &["guest-serial-v2", "nci-v2", "tag-control-v2"],
        ),
        formal_tool_status(
            true,
            tools,
            "uwb-adapter",
            "uwb-adapter",
            &[
                "guest-serial-v2",
                "uwb-control-v2",
                "deterministic-ranging-v2",
                "runtime-ranging-control-v2",
            ],
        ),
        formal_tool_status(
            true,
            tools,
            "modem-adapter",
            "modem-adapter",
            &[
                "guest-vsock-v2",
                "modem-control-v2",
                "at-command-baseline-v2",
                "runtime-modem-state-v2",
                "runtime-modem-unsolicited-v2",
            ],
        ),
    );
    let network = artifact_roles_status(tools, &["crosvm", "hd-device-sim"]);
    let audio = audio_artifact_status(tools);
    let camera = artifact_roles_status(tools, &["crosvm", "gfxstream-backend"]);
    let statuses = constrain_host_device_capabilities(FormalDeviceStatuses {
        simulator,
        sensor_injector: artifact_file_status(sensor_injector, "sensor-injector"),
        bluetooth,
        nfc,
        uwb,
        modem,
        network,
        audio,
        camera,
    });
    compose_device_capabilities(&statuses)
}

fn fast_device_capabilities(
    tools: &BTreeMap<String, PathBuf>,
    sensor_injector: &Path,
) -> DeviceCapabilitiesV2 {
    let statuses = constrain_host_device_capabilities(FormalDeviceStatuses {
        simulator: fast_formal_tool_status(true, tools, "hd-device-sim"),
        sensor_injector: artifact_file_status(sensor_injector, "sensor-injector"),
        bluetooth: fast_formal_tool_status(true, tools, "rootcanal-adapter"),
        nfc: fast_formal_tool_status(true, tools, "casimir-adapter"),
        uwb: fast_formal_tool_status(true, tools, "uwb-adapter"),
        modem: fast_formal_tool_status(true, tools, "modem-adapter"),
        network: artifact_roles_status(tools, &["crosvm", "hd-device-sim"]),
        audio: audio_artifact_status(tools),
        camera: artifact_roles_status(tools, &["crosvm", "gfxstream-backend"]),
    });
    compose_device_capabilities(&statuses)
}

#[derive(Debug)]
struct FormalDeviceStatuses {
    simulator: FormalToolStatus,
    sensor_injector: FormalToolStatus,
    bluetooth: FormalToolStatus,
    nfc: FormalToolStatus,
    uwb: FormalToolStatus,
    modem: FormalToolStatus,
    network: FormalToolStatus,
    audio: FormalToolStatus,
    camera: FormalToolStatus,
}

fn constrain_host_device_capabilities(statuses: FormalDeviceStatuses) -> FormalDeviceStatuses {
    #[cfg(target_os = "macos")]
    let statuses = {
        let mut statuses = statuses;
        let guest_software_surface = statuses.network.available;
        let audio_available = statuses.audio.available;
        statuses.network = if crate::backend::macos_socket_vmnet_path().is_some() {
            software_on_macos(
                guest_software_surface,
                "Android Connectivity stack with a shared NAT uplink through socket_vmnet; full-tunnel VPN support requires the verified macOS network service shown in Devices",
            )
        } else {
            software_on_macos(
                false,
                "Android Connectivity and wireless simulation stack in an offline profile",
            )
        };
        statuses.audio = software_on_macos(
            audio_available,
            "Android AIDL Audio HAL and crosvm virtio-snd output routed to CoreAudio Host playback; Host microphone capture remains disabled",
        );
        statuses.camera = software_on_macos(
            guest_software_surface,
            "Android virtual camera provider with deterministic internal test sources",
        );
        statuses
    };
    statuses
}

#[cfg(target_os = "macos")]
fn software_on_macos(available: bool, detail: &str) -> FormalToolStatus {
    FormalToolStatus {
        available,
        detail: format!(
            "macOS Guest software simulation: {detail}; profile artifacts {}",
            if available {
                "are configured"
            } else {
                "are incomplete"
            }
        ),
    }
}

fn compose_device_capabilities(statuses: &FormalDeviceStatuses) -> DeviceCapabilitiesV2 {
    let mut devices = primary_device_capabilities(statuses);
    devices.extend(simulated_device_capabilities(statuses));
    #[cfg(target_os = "macos")]
    apply_macos_device_boundaries(&mut devices);
    #[cfg(not(target_os = "macos"))]
    for device in &mut devices {
        if matches!(
            device.id.as_str(),
            "bluetooth" | "nfc" | "uwb" | "gnss" | "sensors" | "network" | "power"
        ) && !device
            .features
            .iter()
            .any(|feature| feature == "runtime_control")
        {
            device.features.push("runtime_control".to_owned());
        }
    }
    DeviceCapabilitiesV2 {
        profile: "hd-phone-android15-v2".to_owned(),
        devices,
    }
}

#[cfg(target_os = "macos")]
const MACOS_BLUETOOTH_FEATURES: &[&str] = &[
    "hci",
    "le_scan",
    "le_advertise",
    "gatt",
    "beacon",
    "scripted_beacon",
    "basic_pairing",
    "hci_capture",
    "runtime_control",
];

#[cfg(target_os = "macos")]
const MACOS_UWB_FEATURES: &[&str] = &[
    "aidl_hal",
    "uci_2",
    "fira_v2",
    "deterministic_ranging",
    "runtime_control",
];

#[cfg(target_os = "macos")]
fn apply_macos_device_boundaries(devices: &mut [DeviceCapabilityV2]) {
    let macos_network_uplink = crate::backend::macos_socket_vmnet_path().is_some();
    for device in devices {
        if device.available {
            device.backend = if matches!(device.id.as_str(), "bluetooth" | "nfc" | "uwb" | "modem")
            {
                DeviceBackendKindV2::OfficialComponent
            } else {
                DeviceBackendKindV2::SoftwareBacked
            };
        }
        let (boundary, features): (&str, &[&str]) = match device.id.as_str() {
            "bluetooth" => (
                "AOSP RootCanal HCI with deterministic virtual BLE peers and bounded per-instance btsnoop capture; no physical RF claim",
                MACOS_BLUETOOTH_FEATURES,
            ),
            "nfc" => (
                "AOSP Casimir NCI 2.0 with deterministic Type 2/4 NDEF tag injection and Android HCE; no secure-element or physical RF claim",
                &[
                    "nci_2",
                    "ndef_type2",
                    "ndef_type4",
                    "hce",
                    "runtime_control",
                ],
            ),
            "uwb" => (
                "formal deterministic FiRa v2 UCI controller with session lifecycle and short-address distance reports; no physical RF, angle, or CCC conformance claim",
                MACOS_UWB_FEATURES,
            ),
            "modem" => (
                "formal deterministic AT modem over Guest-to-Host vsock with test operator, SIM identity, signal and registration queries; no physical carrier, call, SMS, IMS or data-attach claim",
                &[
                    "radio_hal",
                    "guest_vsock",
                    "basic_at",
                    "signal_query",
                    "operator_query",
                    "registration_query",
                    "runtime_control",
                ],
            ),
            "gnss" => (
                "Android LocationManager test provider with deterministic coordinate injection and verification",
                &["location", "runtime_control"],
            ),
            "sensors" => (
                "Android 15 AOSP Guest motion injector applies one coherent accelerometer, magnetometer and gyroscope frame derived from three-axis pose; independent and timed sensor overrides are not published for this profile",
                &["three_axis_pose", "runtime_control"],
            ),
            "network" if macos_network_uplink => (
                "Android Connectivity stack with a shared NAT uplink through socket_vmnet and deterministic runtime traffic shaping; full-tunnel VPN support requires the verified macOS network service shown in Devices; no Wi-Fi RF or port-forward claim",
                &["connectivity_stack", "shared_nat_uplink", "runtime_control"],
            ),
            "network" => (
                "Android Connectivity stack in an offline profile with deterministic runtime traffic shaping; no host uplink or Wi-Fi RF claim",
                &["connectivity_stack", "offline_profile", "runtime_control"],
            ),
            "audio" => (
                "Android AudioFlinger over crosvm virtio-snd with CoreAudio Host playback; no Host microphone capture or physical input-device claim",
                &["aidl_audio_hal", "virtio_snd", "host_playback"],
            ),
            "camera" => (
                "Android virtual camera provider with deterministic internal test sources; no physical host-camera claim",
                &["preview", "jpeg_capture", "virtual_test_source"],
            ),
            "power" => (
                "Android BatteryService software state with typed host injection and verification",
                &["battery", "runtime_control"],
            ),
            _ => continue,
        };
        boundary.clone_into(&mut device.boundary);
        device.features = unique_features(features);
        device.runtime.controllable = device
            .features
            .iter()
            .any(|feature| feature == "runtime_control");
        device.runtime.detail = if device.available {
            "profile is configured; select a running instance for runtime verification".to_owned()
        } else {
            "profile artifacts are incomplete".to_owned()
        };
    }
}

fn primary_device_capabilities(statuses: &FormalDeviceStatuses) -> Vec<DeviceCapabilityV2> {
    vec![
        device(
            "bluetooth",
            DeviceBackendKindV2::OfficialComponent,
            statuses.bluetooth.available,
            &component_boundary(
                "RootCanal HCI with deterministic virtual peers; no physical RF claim",
                &statuses.bluetooth,
            ),
            &[
                "hci",
                "le_scan",
                "le_advertise",
                "gatt",
                "beacon",
                "scripted_beacon",
                "basic_pairing",
                "hci_capture",
                "runtime_control",
            ],
        ),
        device(
            "nfc",
            DeviceBackendKindV2::OfficialComponent,
            statuses.nfc.available,
            &component_boundary(
                "Casimir NCI 2.0 with Type 2/4 NDEF and HCE; no secure-element claim",
                &statuses.nfc,
            ),
            &["nci_2", "ndef_type2", "ndef_type4", "hce"],
        ),
        device(
            "uwb",
            DeviceBackendKindV2::OfficialComponent,
            statuses.uwb.available,
            &component_boundary(
                "deterministic FiRa v2 session lifecycle and short-address distance reports; no angle, RF, or CCC conformance claim",
                &statuses.uwb,
            ),
            &["guest_control", "deterministic_reply"],
        ),
        device(
            "modem",
            DeviceBackendKindV2::OfficialComponent,
            statuses.modem.available,
            &component_boundary(
                "deterministic basic AT signal/operator/registration baseline; no data attach, SMS, calls, IMS, 5G, or carrier conformance claim",
                &statuses.modem,
            ),
            &[
                "basic_at",
                "signal_query",
                "operator_query",
                "registration_query",
                "runtime_control",
            ],
        ),
    ]
}

fn simulated_device_capabilities(statuses: &FormalDeviceStatuses) -> Vec<DeviceCapabilityV2> {
    let desktop_guest_motion = cfg!(any(target_os = "macos", windows));
    let sensor_boundary = if desktop_guest_motion {
        format!(
            "Android 15 AOSP Guest motion injector for one coherent accelerometer, magnetometer and gyroscope pose frame; independent and timed overrides are unavailable in this profile; simulator: {}",
            statuses.simulator.detail
        )
    } else {
        format!(
            "Android SensorService HAL-bypass injection for the fixed five-sensor manifest with timed override reset; simulator: {}; Guest injector: {}",
            statuses.simulator.detail, statuses.sensor_injector.detail
        )
    };
    let sensor_features: &[&str] = if desktop_guest_motion {
        &["three_axis_pose"]
    } else {
        &[
            "accelerometer",
            "gyroscope",
            "magnetometer",
            "light",
            "proximity",
            "timed_override_reset",
            "three_axis_pose",
        ]
    };
    vec![
        device(
            "gnss",
            DeviceBackendKindV2::Simulated,
            statuses.simulator.available,
            &component_boundary("deterministic coordinate injection", &statuses.simulator),
            &["location"],
        ),
        device(
            "sensors",
            DeviceBackendKindV2::Simulated,
            statuses.simulator.available
                && (cfg!(any(target_os = "macos", windows)) || statuses.sensor_injector.available),
            &sensor_boundary,
            sensor_features,
        ),
        device(
            "network",
            DeviceBackendKindV2::Simulated,
            statuses.network.available,
            &component_boundary(
                "two fixed crosvm virtio-net interfaces with typed latency/loss/rate shaping applied and verified through Android tc netem; no port-forward or Wi-Fi RF claim",
                &statuses.network,
            ),
            &["virtio_net", "guest_control"],
        ),
        device(
            "audio",
            DeviceBackendKindV2::Simulated,
            statuses.audio.available,
            &component_boundary(audio_capability_boundary(), &statuses.audio),
            audio_capability_features(),
        ),
        device(
            "camera",
            DeviceBackendKindV2::Simulated,
            statuses.camera.available,
            &component_boundary(
                "Android camera provider HAL with deterministic virtual test sources rendered through the Guest graphics stack; no host camera claim",
                &statuses.camera,
            ),
            &["preview", "jpeg_capture", "virtual_test_source"],
        ),
        device(
            "power",
            DeviceBackendKindV2::Simulated,
            statuses.simulator.available,
            &component_boundary(
                "typed battery state applied and verified through Android BatteryService",
                &statuses.simulator,
            ),
            &["battery"],
        ),
        device(
            "touchpad",
            DeviceBackendKindV2::Unsupported,
            false,
            "Host touchpad gesture emulation is not exposed; mouse input is delivered directly through the native Android display",
            &[],
        ),
    ]
}

fn audio_capability_features() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["virtio_snd", "host_playback", "host_default_microphone"]
    }
    #[cfg(target_os = "macos")]
    {
        &["virtio_snd", "host_playback"]
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        &["virtio_snd_null", "playback", "capture"]
    }
}

fn audio_capability_boundary() -> &'static str {
    #[cfg(windows)]
    {
        "Android AudioFlinger over crosvm virtio-snd with WASAPI Host playback; the system default Host microphone is routed only when explicitly enabled for this instance"
    }
    #[cfg(target_os = "macos")]
    {
        "Android AudioFlinger over crosvm virtio-snd with CoreAudio Host playback; Host microphone capture is not exposed until per-instance TCC permission and capture evidence are complete"
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        "Android AudioFlinger over crosvm virtio-snd null playback/capture endpoints; no host-device audio claim"
    }
}

fn audio_artifact_status(tools: &BTreeMap<String, PathBuf>) -> FormalToolStatus {
    #[cfg(windows)]
    {
        artifact_roles_status(tools, &["crosvm", "crosvm-runtime-audio"])
    }
    #[cfg(target_os = "macos")]
    {
        // CoreAudio is linked into crosvm; there is no separate runtime or placeholder artifact.
        artifact_roles_status(tools, &["crosvm"])
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        artifact_roles_status(tools, &["crosvm"])
    }
}

#[derive(Debug)]
struct FormalToolStatus {
    available: bool,
    detail: String,
}

fn artifact_file_status(path: &Path, role: &str) -> FormalToolStatus {
    if !path.is_file() {
        return FormalToolStatus {
            available: false,
            detail: format!("artifact role {role} is missing: {}", path.display()),
        };
    }
    FormalToolStatus {
        available: true,
        detail: format!("verified artifact role {role} at {}", path.display()),
    }
}

fn artifact_roles_status(tools: &BTreeMap<String, PathBuf>, roles: &[&str]) -> FormalToolStatus {
    let missing = roles
        .iter()
        .filter_map(|role| match tools.get(*role) {
            Some(path) if path.is_file() => None,
            Some(path) => Some(format!("{role} ({})", path.display())),
            None => Some((*role).to_owned()),
        })
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return FormalToolStatus {
            available: false,
            detail: format!(
                "required artifact roles are missing: {}",
                missing.join(", ")
            ),
        };
    }
    FormalToolStatus {
        available: true,
        detail: format!("verified artifact roles {}", roles.join(", ")),
    }
}

fn fast_formal_tool_status(
    required: bool,
    tools: &BTreeMap<String, PathBuf>,
    role: &str,
) -> FormalToolStatus {
    if !required {
        return FormalToolStatus {
            available: true,
            detail: "disabled by the instance profile".to_owned(),
        };
    }
    let Some(path) = tools.get(role) else {
        return FormalToolStatus {
            available: false,
            detail: format!("signed host bundle is missing role {role}"),
        };
    };
    if !path.is_file() {
        return FormalToolStatus {
            available: false,
            detail: format!("component adapter is missing: {}", path.display()),
        };
    }
    FormalToolStatus {
        available: true,
        detail: format!(
            "dev fast capability mode trusts manifest role {role} at {} without running component probe",
            path.display()
        ),
    }
}

async fn formal_tool_status(
    required: bool,
    tools: &BTreeMap<String, PathBuf>,
    role: &str,
    component: &str,
    required_features: &[&str],
) -> FormalToolStatus {
    if !required {
        return FormalToolStatus {
            available: true,
            detail: "disabled by the instance profile".to_owned(),
        };
    }
    let Some(path) = tools.get(role) else {
        return FormalToolStatus {
            available: false,
            detail: format!("signed host bundle is missing role {role}"),
        };
    };
    if !path.is_file() {
        return FormalToolStatus {
            available: false,
            detail: format!("component adapter is missing: {}", path.display()),
        };
    }
    match run_json_probe::<FormalComponentProbeV2>(path, &["--probe-v2", "--json"]).await {
        Ok(probe) => {
            let missing = required_features
                .iter()
                .filter(|feature| !probe.features.iter().any(|actual| actual == **feature))
                .copied()
                .collect::<Vec<_>>();
            if probe.protocol_version != COMPONENT_PROTOCOL_VERSION
                || probe.component != component
                || !probe.formal
                || !missing.is_empty()
            {
                FormalToolStatus {
                    available: false,
                    detail: format!(
                        "component probe contract mismatch (component={}, protocol={}, formal={}, missing={})",
                        probe.component,
                        probe.protocol_version,
                        probe.formal,
                        missing.join(",")
                    ),
                }
            } else {
                FormalToolStatus {
                    available: true,
                    detail: format!("verified adapter {}", path.display()),
                }
            }
        }
        Err(error) => FormalToolStatus {
            available: false,
            detail: error,
        },
    }
}

fn component_boundary(boundary: &str, status: &FormalToolStatus) -> String {
    format!("{boundary}; probe: {}", status.detail)
}

fn formal_component_capability_probe(
    id: &str,
    required: bool,
    status: &FormalToolStatus,
) -> CapabilityProbeV2 {
    if status.available {
        supported_probe(id, required, status.detail.clone(), BTreeMap::new())
    } else {
        blocked_probe(id, required, status.detail.clone(), BTreeMap::new())
    }
}

fn unavailable_devices(reason: &str) -> DeviceCapabilitiesV2 {
    DeviceCapabilitiesV2 {
        profile: "hd-phone-android15-v2".to_owned(),
        devices: [
            "bluetooth",
            "nfc",
            "uwb",
            "modem",
            "gnss",
            "sensors",
            "network",
            "audio",
            "camera",
            "power",
            "touchpad",
        ]
        .into_iter()
        .map(|id| DeviceCapabilityV2 {
            id: id.to_owned(),
            backend: DeviceBackendKindV2::Unsupported,
            available: false,
            boundary: reason.to_owned(),
            features: Vec::new(),
            runtime: hd_core::DeviceRuntimeStateV2::default(),
        })
        .collect(),
    }
}

fn device(
    id: &str,
    backend: DeviceBackendKindV2,
    available: bool,
    boundary: &str,
    features: &[&str],
) -> DeviceCapabilityV2 {
    let controllable = features.contains(&"runtime_control");
    DeviceCapabilityV2 {
        id: id.to_owned(),
        backend,
        available,
        boundary: boundary.to_owned(),
        features: unique_features(features),
        runtime: hd_core::DeviceRuntimeStateV2 {
            probed: false,
            installed: available,
            configured: available,
            running: false,
            controllable,
            verified: false,
            detail: if available {
                "profile is configured; runtime verification requires a running instance".to_owned()
            } else {
                boundary.to_owned()
            },
        },
    }
}

fn unique_features(features: &[&str]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    features
        .iter()
        .copied()
        .filter(|feature| seen.insert(*feature))
        .map(str::to_owned)
        .collect()
}

fn supported_probe(
    id: &str,
    required: bool,
    detail: impl Into<String>,
    properties: BTreeMap<String, String>,
) -> CapabilityProbeV2 {
    CapabilityProbeV2 {
        id: id.to_owned(),
        status: CapabilityStatusV2::Supported,
        required,
        detail: detail.into(),
        properties,
    }
}

fn fast_supported_probe(
    id: &str,
    required: bool,
    detail: impl Into<String>,
    mut properties: BTreeMap<String, String>,
) -> CapabilityProbeV2 {
    properties.insert("mode".to_owned(), "dev_fast_capabilities".to_owned());
    supported_probe(id, required, detail, properties)
}

fn blocked_probe(
    id: &str,
    required: bool,
    detail: impl Into<String>,
    properties: BTreeMap<String, String>,
) -> CapabilityProbeV2 {
    CapabilityProbeV2 {
        id: id.to_owned(),
        status: CapabilityStatusV2::Blocked,
        required,
        detail: detail.into(),
        properties,
    }
}

fn capability_fingerprint(probes: &[CapabilityProbeV2], devices: &DeviceCapabilitiesV2) -> String {
    #[derive(Serialize)]
    struct Fingerprint<'a> {
        platform: &'a str,
        architecture: &'a str,
        probes: Vec<CapabilityProbeV2>,
        devices: StableDeviceCapabilities<'a>,
    }
    let payload = serde_json::to_vec(&Fingerprint {
        platform: platform_name(),
        architecture: architecture_name(),
        probes: normalized_probes(probes, false),
        devices: stable_device_capabilities(devices),
    })
    .unwrap_or_default();
    hex::encode(Sha256::digest(payload))
}

fn release_fingerprint(
    probes: &[CapabilityProbeV2],
    devices: &DeviceCapabilitiesV2,
    portable_paths: bool,
) -> String {
    #[derive(Serialize)]
    struct Fingerprint<'a> {
        platform: &'a str,
        architecture: &'a str,
        probes: Vec<CapabilityProbeV2>,
        devices: StableDeviceCapabilities<'a>,
    }
    let mut probes = normalized_probes(probes, true);
    if portable_paths {
        for probe in &mut probes {
            probe.properties.remove("path");
            probe.properties.remove("environment");
        }
    }
    let payload = serde_json::to_vec(&Fingerprint {
        platform: platform_name(),
        architecture: architecture_name(),
        probes,
        devices: stable_device_capabilities(devices),
    })
    .unwrap_or_default();
    hex::encode(Sha256::digest(payload))
}

#[derive(Serialize)]
struct StableDeviceCapabilities<'a> {
    profile: &'a str,
    devices: Vec<StableDeviceCapability<'a>>,
}

#[derive(Serialize)]
struct StableDeviceCapability<'a> {
    id: &'a str,
    backend: DeviceBackendKindV2,
    available: bool,
    boundary: &'a str,
    features: &'a [String],
}

fn stable_device_capabilities(devices: &DeviceCapabilitiesV2) -> StableDeviceCapabilities<'_> {
    StableDeviceCapabilities {
        profile: &devices.profile,
        devices: devices
            .devices
            .iter()
            .map(|device| StableDeviceCapability {
                id: &device.id,
                backend: device.backend,
                available: device.available,
                boundary: &device.boundary,
                features: &device.features,
            })
            .collect(),
    }
}

fn normalized_probes(
    probes: &[CapabilityProbeV2],
    release_identity: bool,
) -> Vec<CapabilityProbeV2> {
    probes
        .iter()
        .filter(|probe| {
            !release_identity || !matches!(probe.id.as_str(), "host.resources" | "guest.readiness")
        })
        .cloned()
        .map(|mut probe| {
            if probe.id == "host.resources" {
                probe.properties.remove("available_memory_bytes");
                probe.properties.remove("available_disk_bytes");
            }
            probe
        })
        .collect()
}

fn sibling_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::current_exe()
        .ok()?
        .parent()?
        .join(executable_name(name));
    path.is_file().then_some(path)
}

struct CertificationIdentity<'a> {
    guest_kind: GuestKindV2,
    guest_digest: &'a str,
    host_digest: &'a str,
    device_profile: &'a str,
}

fn certification_matches(
    paths: &DataPaths,
    spec: Option<&InstanceSpecV2>,
    identity: &CertificationIdentity<'_>,
    capability_fingerprint: &str,
    trust: Option<&ArtifactTrustStore>,
) -> Result<HostCertificationV2, String> {
    const ANDROID_EVIDENCE: [&str; 8] = [
        "hd_quality",
        "host_worker_smoke",
        "http_security_smoke",
        "lease_recovery_smoke",
        "diagnostic_smoke",
        "real_guest",
        "zero_copy",
        "device_profile_conformance",
    ];
    const MICRODROID_EVIDENCE: [&str; 8] = [
        "hd_quality",
        "host_worker_smoke",
        "http_security_smoke",
        "lease_recovery_smoke",
        "diagnostic_smoke",
        "microdroid_real_guest",
        "microdroid_multi_instance",
        "microdroid_payload_conformance",
    ];
    let spec = spec.ok_or_else(|| "select an instance before certification lookup".to_owned())?;
    if spec.guest_kind != identity.guest_kind {
        return Err("certification guest kind does not match the selected instance".to_owned());
    }
    let trust = trust.ok_or_else(|| "artifact trust store is unavailable".to_owned())?;
    let path = paths.host_certification(
        platform_name(),
        architecture_name(),
        identity.guest_digest,
        identity.host_digest,
    );
    let bytes = read_certification(&path)?;
    let certification: HostCertificationV2 = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode {}: {error}", path.display()))?;
    if certification.schema_version != HOST_CERTIFICATION_VERSION
        || certification.platform != platform_name()
        || certification.architecture != architecture_name()
        || certification.guest_kind != identity.guest_kind
        || certification.capability_fingerprint != capability_fingerprint
        || certification.guest_bundle_digest != identity.guest_digest
        || certification.host_bundle_digest != identity.host_digest
        || certification.device_profile != identity.device_profile
        || certification.control_protocol_version != CONTROL_PROTOCOL_VERSION
        || certification.frame_protocol_version != FRAME_PROTOCOL_VERSION
    {
        return Err("certification identity does not match this host and bundle".to_owned());
    }
    let now = OffsetDateTime::now_utc();
    if certification.issued_at > now + time::Duration::minutes(5)
        || certification.expires_at <= now
        || certification.expires_at - certification.issued_at > time::Duration::days(31)
    {
        return Err("certification validity window is invalid or expired".to_owned());
    }
    let required_evidence = match identity.guest_kind {
        GuestKindV2::Android => ANDROID_EVIDENCE.as_slice(),
        GuestKindV2::Microdroid => MICRODROID_EVIDENCE.as_slice(),
    };
    for gate in required_evidence {
        let digest = certification
            .evidence_sha256
            .get(*gate)
            .ok_or_else(|| format!("certification is missing {gate} evidence"))?;
        if !valid_digest(digest) {
            return Err(format!(
                "certification evidence digest for {gate} is invalid"
            ));
        }
    }
    if certification.evidence_sha256.len() != required_evidence.len() {
        return Err("certification contains an unrecognized evidence gate".to_owned());
    }
    let payload = certification_payload(&certification)?;
    trust
        .verify_detached(
            &certification.signer_key_id,
            &certification.signature_ed25519,
            &payload,
        )
        .map_err(|error| format!("verify certification signature: {error}"))?;
    Ok(certification)
}

fn certification_payload(certification: &HostCertificationV2) -> Result<Vec<u8>, String> {
    let mut unsigned = certification.clone();
    unsigned.signature_ed25519.clear();
    serde_json::to_vec(&unsigned).map_err(|error| format!("encode certification payload: {error}"))
}

fn read_certification(path: &Path) -> Result<Vec<u8>, String> {
    const LIMIT: u64 = 256 * 1024;
    hd_platform::read_regular_nofollow_limited(path, LIMIT)
        .map_err(|error| format!("read certification {}: {error}", path.display()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn expected_frame_transport() -> FrameTransportKindV2 {
    if cfg!(windows) {
        FrameTransportKindV2::VulkanWin32
    } else if cfg!(target_os = "linux") {
        FrameTransportKindV2::VulkanDmaBuf
    } else {
        FrameTransportKindV2::MetalIoSurface
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resources_probe(available: &str, requested: &str) -> CapabilityProbeV2 {
        supported_probe(
            "host.resources",
            true,
            "admission passed",
            BTreeMap::from([
                ("available_memory_bytes".to_owned(), available.to_owned()),
                ("available_disk_bytes".to_owned(), available.to_owned()),
                ("requested_memory_bytes".to_owned(), requested.to_owned()),
            ]),
        )
    }

    #[test]
    fn dynamic_capacity_does_not_invalidate_identity() {
        let devices = DeviceCapabilitiesV2 {
            profile: "hd-phone-android15-v2".to_owned(),
            devices: Vec::new(),
        };
        let first = vec![resources_probe("100", "50")];
        let capacity_changed = vec![resources_probe("99", "50")];
        let request_changed = vec![resources_probe("100", "51")];
        assert_eq!(
            capability_fingerprint(&first, &devices),
            capability_fingerprint(&capacity_changed, &devices)
        );
        assert_ne!(
            capability_fingerprint(&first, &devices),
            capability_fingerprint(&request_changed, &devices)
        );
        assert_eq!(
            release_fingerprint(&first, &devices, false),
            release_fingerprint(&request_changed, &devices, false)
        );
    }

    #[test]
    fn microdroid_disk_requirement_tracks_configured_storage() {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;
        let mut microdroid = InstanceSpecV2::microdroid("Microdroid");
        microdroid
            .microdroid
            .as_mut()
            .expect("Microdroid config")
            .encrypted_storage_mib = Some(128);

        assert_eq!(required_runtime_disk_bytes(None), 10 * GIB);
        assert_eq!(
            required_runtime_disk_bytes(Some(&microdroid)),
            GIB + 128 * MIB
        );
    }

    #[test]
    fn certification_renewal_requires_the_same_identity() {
        let current = HostCertificationV2 {
            schema_version: HOST_CERTIFICATION_VERSION,
            certification_id: uuid::Uuid::nil(),
            platform: "macos".to_owned(),
            architecture: "arm64".to_owned(),
            guest_kind: GuestKindV2::Microdroid,
            capability_fingerprint: "00".repeat(32),
            guest_bundle_digest: "11".repeat(32),
            host_bundle_digest: "22".repeat(32),
            device_profile: "hd-microdroid-macos-arm64-v2".to_owned(),
            control_protocol_version: CONTROL_PROTOCOL_VERSION,
            frame_protocol_version: FRAME_PROTOCOL_VERSION,
            issued_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(14),
            evidence_sha256: BTreeMap::new(),
            signer_key_id: "release".to_owned(),
            signature_ed25519: "old".to_owned(),
        };
        let mut renewal = current.clone();
        renewal.certification_id = uuid::Uuid::new_v4();
        renewal.issued_at += time::Duration::days(7);
        renewal.expires_at += time::Duration::days(7);
        renewal.signature_ed25519 = "new".to_owned();
        assert!(same_certification_identity(&current, &renewal));

        renewal.guest_kind = GuestKindV2::Android;
        assert!(!same_certification_identity(&current, &renewal));
    }
}

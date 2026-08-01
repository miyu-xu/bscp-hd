use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use hd_core::{
    ArtifactSelectionV2, COMPONENT_PROTOCOL_VERSION, CONTROL_PROTOCOL_VERSION, CapabilityProbeV2,
    CapabilityStatusV2, DeviceBackendKindV2, DeviceCapabilitiesV2, DeviceCapabilityV2,
    FRAME_PROTOCOL_VERSION, FormalComponentProbeV2, FrameTransportKindV2,
    HOST_CERTIFICATION_VERSION, HostCapabilitiesV2, HostCertificationV2, InstanceSpecV2,
};
use hd_platform::{
    DataPaths, FrameInteropProbeV2, architecture_name, configure_managed_command, contain_process,
    executable_name, hypervisor_available as native_hypervisor_available,
    platform_baseline as native_platform_baseline, platform_name, resume_managed_process,
};
use parking_lot::RwLock as ParkingRwLock;
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::leases::{guest_cpu_capacity, host_cpu_reserve};
use crate::{ArtifactResolver, ArtifactTrustStore, ResolvedBundleSetV2};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;

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
        probes.push(resource_probe(&self.paths, spec));

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
                            if let Some(bundle_adb) = bundle_set.artifacts.host_tools.get("adb") {
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
                                    "verified host bundle has no frame-producer role".to_owned(),
                                    BTreeMap::new(),
                                ));
                            }
                            let adb_required = true;
                            adb_bridge = bundle_set.artifacts.host_tools.get("adb-bridge").cloned();
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

        let evidence_fingerprint = release_fingerprint(&probes, &devices);
        let development_bypass = fast_artifacts && spec.is_some();
        let verified = !fast_artifacts
            && !fast_capabilities
            && !dev_display_copy_fallback
            && probes.iter().all(|probe| {
                !probe.required || matches!(probe.status, CapabilityStatusV2::Supported)
            });
        let certified = if development_bypass {
            probes.push(blocked_probe(
                "release.certification",
                false,
                "release certification was not evaluated because development artifact bypass is active",
                BTreeMap::from([(
                    "evidence_fingerprint".to_owned(),
                    evidence_fingerprint.clone(),
                )])
                .into_iter()
                .chain([("mode".to_owned(), "development_bypass".to_owned())])
                .collect(),
            ));
            false
        } else {
            let certification = certification_matches(
                &self.paths,
                spec,
                &evidence_fingerprint,
                trust.as_ref().ok(),
            );
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

fn resource_probe(paths: &DataPaths, spec: Option<&InstanceSpecV2>) -> CapabilityProbeV2 {
    const GIB: u64 = 1024 * 1024 * 1024;
    let resources = hd_platform::host_resources();
    let logical_cpus = resources.as_ref().map_or(0, |value| value.logical_cpus);
    let host_cpu_reserve = host_cpu_reserve(logical_cpus);
    let guest_cpu_capacity = guest_cpu_capacity(logical_cpus);
    let total_memory_bytes = resources
        .as_ref()
        .map_or(0, |value| value.total_memory_bytes);
    let available_memory_bytes = resources
        .as_ref()
        .map_or(0, |value| value.available_memory_bytes);
    let requested_cpus = spec.map_or(2, |instance| usize::from(instance.cpu_count));
    let requested_memory_bytes = spec.map_or(2 * GIB, |instance| {
        u64::from(instance.memory_mib).saturating_mul(1024 * 1024)
    });
    let host_memory_reserve = (total_memory_bytes / 10).max(GIB);
    let required_memory_bytes = requested_memory_bytes.saturating_add(host_memory_reserve);
    let available_bytes = fs2::available_space(&paths.root).unwrap_or(0);
    let mut properties = BTreeMap::from([
        ("logical_cpus".to_owned(), logical_cpus.to_string()),
        ("host_cpu_reserve".to_owned(), host_cpu_reserve.to_string()),
        (
            "guest_cpu_capacity".to_owned(),
            guest_cpu_capacity.to_string(),
        ),
        ("requested_cpus".to_owned(), requested_cpus.to_string()),
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
            "available_disk_bytes".to_owned(),
            available_bytes.to_string(),
        ),
    ]);
    if let Ok(resources) = &resources {
        properties.insert(
            "memory_source".to_owned(),
            resources.memory_source.to_owned(),
        );
    }
    if resources.is_ok()
        && guest_cpu_capacity >= requested_cpus
        && available_memory_bytes >= required_memory_bytes
        && available_bytes >= 10 * GIB
    {
        supported_probe(
            "host.resources",
            true,
            "requested CPU, memory headroom and disk capacity are available".to_owned(),
            properties,
        )
    } else {
        blocked_probe(
            "host.resources",
            true,
            resources.map_or_else(
                |error| format!("query host resources: {error}"),
                |_| {
                    format!(
                        "requires {requested_cpus} guest CPUs from capacity {guest_cpu_capacity} after reserving {host_cpu_reserve} of {logical_cpus} logical CPUs for the Host, {required_memory_bytes} available memory bytes including host reserve, and 10 GiB free disk"
                    )
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
            _ => true,
        })
    };
    let missing = devices
        .devices
        .iter()
        .filter(|device| enabled(&device.id) && !device.available)
        .map(|device| device.id.clone())
        .collect::<Vec<_>>();
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
            ],
        ),
        formal_tool_status(
            true,
            tools,
            "rootcanal-adapter",
            "rootcanal-adapter",
            &["guest-serial-v2", "hci-v2", "peer-control-v2"],
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
            ],
        ),
    );
    let network = artifact_roles_status(tools, &["crosvm", "hd-device-sim"]);
    let audio = artifact_roles_status(tools, &["crosvm", "crosvm-runtime-audio"]);
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
        audio: artifact_roles_status(tools, &["crosvm", "crosvm-runtime-audio"]),
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

fn constrain_host_device_capabilities(mut statuses: FormalDeviceStatuses) -> FormalDeviceStatuses {
    #[cfg(target_os = "macos")]
    {
        let guest_software_surface = statuses.network.available;
        statuses.bluetooth = software_on_macos(
            guest_software_surface,
            "Android AIDL Bluetooth HCI and framework services; host peer/RF injection is not exposed",
        );
        statuses.nfc = software_on_macos(
            guest_software_surface,
            "Android NFC framework package and HCE surface; host NDEF/RF injection is not exposed",
        );
        statuses.uwb = software_on_macos(
            guest_software_surface,
            "Android UWB framework and AIDL HAL baseline; host ranging injection is not exposed",
        );
        statuses.modem = software_on_macos(
            guest_software_surface,
            "Android Radio AIDL HAL baseline with deterministic out-of-service state",
        );
        statuses.network = if crate::backend::macos_socket_vmnet_path().is_some() {
            software_on_macos(
                guest_software_surface,
                "Android Connectivity stack with a shared NAT uplink through socket_vmnet",
            )
        } else {
            software_on_macos(
                false,
                "Android Connectivity and wireless simulation stack in an offline profile",
            )
        };
        statuses.audio = software_on_macos(
            guest_software_surface,
            "Android AIDL Audio HAL with deterministic virtual speaker and microphone endpoints",
        );
        statuses.camera = software_on_macos(
            guest_software_surface,
            "Android virtual camera provider with deterministic internal test sources",
        );
    }
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
    let macos_network_uplink = crate::backend::macos_socket_vmnet_path().is_some();
    #[cfg(target_os = "macos")]
    for device in &mut devices {
        if device.available {
            device.backend = DeviceBackendKindV2::SoftwareBacked;
        }
        let (boundary, features): (&str, &[&str]) = match device.id.as_str() {
            "bluetooth" => (
                "Android AIDL Bluetooth HCI and framework software surface; no physical RF or host peer injection claim",
                &["hci_service", "framework_state"],
            ),
            "nfc" => (
                "Android NFC framework and HCE software surface; no secure-element, physical RF, or host NDEF injection claim",
                &["framework_state", "hce"],
            ),
            "uwb" => (
                "Android UWB framework and AIDL HAL software baseline; no physical ranging, angle, RF, or host session injection claim",
                &["framework_state", "aidl_hal"],
            ),
            "modem" => (
                "Android Radio AIDL HAL software baseline with deterministic out-of-service state; no carrier, call, SMS, IMS, or data-attach claim",
                &["radio_hal", "service_state"],
            ),
            "gnss" => (
                "Android LocationManager test provider with deterministic coordinate injection and verification",
                &["location", "runtime_control"],
            ),
            "sensors" => (
                "Android Guest software Sensors HAL with deterministic host runtime injection for accelerometer, gyroscope, magnetometer, light and proximity",
                &[
                    "accelerometer",
                    "gyroscope",
                    "magnetometer",
                    "light",
                    "proximity",
                    "runtime_control",
                ],
            ),
            "network" if macos_network_uplink => (
                "Android Connectivity stack with a shared NAT uplink through socket_vmnet and deterministic runtime traffic shaping; no Wi-Fi RF or port-forward claim",
                &["connectivity_stack", "shared_nat_uplink", "runtime_control"],
            ),
            "network" => (
                "Android Connectivity stack in an offline profile with deterministic runtime traffic shaping; no host uplink or Wi-Fi RF claim",
                &["connectivity_stack", "offline_profile", "runtime_control"],
            ),
            "audio" => (
                "Android AIDL Audio HAL with deterministic virtual speaker and microphone endpoints; no physical host-device audio claim",
                &["aidl_audio_hal", "playback", "capture", "virtual_endpoints"],
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
        device.features = features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect();
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
    #[cfg(not(target_os = "macos"))]
    for device in &mut devices {
        if matches!(
            device.id.as_str(),
            "bluetooth" | "nfc" | "gnss" | "sensors" | "network" | "power"
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
            &["hci", "le_scan", "le_advertise", "gatt", "basic_pairing"],
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
            DeviceBackendKindV2::Simulated,
            statuses.uwb.available,
            &component_boundary(
                "deterministic FiRa v2 session lifecycle and short-address distance reports; no angle, RF, or CCC conformance claim",
                &statuses.uwb,
            ),
            &["guest_control", "deterministic_reply"],
        ),
        device(
            "modem",
            DeviceBackendKindV2::Simulated,
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
            ],
        ),
    ]
}

fn simulated_device_capabilities(statuses: &FormalDeviceStatuses) -> Vec<DeviceCapabilityV2> {
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
                && (cfg!(target_os = "macos") || statuses.sensor_injector.available),
            &if cfg!(target_os = "macos") {
                format!(
                    "Android Sensors HAL hvc13 bridge for the fixed five-sensor manifest; simulator: {}",
                    statuses.simulator.detail
                )
            } else {
                format!(
                    "Android SensorService HAL-bypass injection for the fixed five-sensor manifest; simulator: {}; Guest injector: {}",
                    statuses.simulator.detail, statuses.sensor_injector.detail
                )
            },
            &[
                "accelerometer",
                "gyroscope",
                "magnetometer",
                "light",
                "proximity",
            ],
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
            &component_boundary(
                "Android AudioFlinger over crosvm virtio-snd null playback/capture endpoints; no host-device audio claim",
                &statuses.audio,
            ),
            &["virtio_snd_null", "playback", "capture"],
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
    ]
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
        features: features.iter().map(|value| (*value).to_owned()).collect(),
        runtime: hd_core::DeviceRuntimeStateV2 {
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

fn release_fingerprint(probes: &[CapabilityProbeV2], devices: &DeviceCapabilitiesV2) -> String {
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
        probes: normalized_probes(probes, true),
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

fn certification_matches(
    paths: &DataPaths,
    spec: Option<&InstanceSpecV2>,
    capability_fingerprint: &str,
    trust: Option<&ArtifactTrustStore>,
) -> Result<HostCertificationV2, String> {
    const REQUIRED_EVIDENCE: [&str; 8] = [
        "hd_quality",
        "host_worker_smoke",
        "http_security_smoke",
        "lease_recovery_smoke",
        "diagnostic_smoke",
        "real_guest",
        "zero_copy",
        "device_profile_conformance",
    ];
    let spec = spec.ok_or_else(|| "select an instance before certification lookup".to_owned())?;
    let selection = spec
        .artifacts
        .as_ref()
        .ok_or_else(|| "instance has no signed bundle selection".to_owned())?;
    let trust = trust.ok_or_else(|| "artifact trust store is unavailable".to_owned())?;
    let path = paths.host_certification(
        platform_name(),
        architecture_name(),
        &selection.guest_bundle_digest,
        &selection.host_bundle_digest,
    );
    let bytes = read_certification(&path)?;
    let certification: HostCertificationV2 = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode {}: {error}", path.display()))?;
    if certification.schema_version != HOST_CERTIFICATION_VERSION
        || certification.platform != platform_name()
        || certification.architecture != architecture_name()
        || certification.capability_fingerprint != capability_fingerprint
        || certification.guest_bundle_digest != selection.guest_bundle_digest
        || certification.host_bundle_digest != selection.host_bundle_digest
        || certification.device_profile != "hd-phone-android15-v2"
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
    for gate in REQUIRED_EVIDENCE {
        let digest = certification
            .evidence_sha256
            .get(gate)
            .ok_or_else(|| format!("certification is missing {gate} evidence"))?;
        if !valid_digest(digest) {
            return Err(format!(
                "certification evidence digest for {gate} is invalid"
            ));
        }
    }
    if certification.evidence_sha256.len() != REQUIRED_EVIDENCE.len() {
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
            release_fingerprint(&first, &devices),
            release_fingerprint(&request_changed, &devices)
        );
    }
}

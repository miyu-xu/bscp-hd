use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hd_core::{
    ARTIFACT_INDEX_VERSION, ArtifactBundleKindV2, ArtifactBundleV2, ArtifactFileV2,
    ArtifactReadyMarkerV2, ArtifactSelectionV2, DiagnosticCheckV2, DiagnosticStatusV2,
    ResolvedGuestArtifactsV2,
};
use hd_platform::DataPaths;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;

const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MANIFEST_FILE: &str = "manifest-v2.json";
const READY_FILE: &str = "READY-v2.json";

#[derive(Debug, Clone)]
pub struct ArtifactTrustStore {
    keys: BTreeMap<String, VerifyingKey>,
}

impl ArtifactTrustStore {
    pub fn load(path: &Path) -> Result<Self, ArtifactError> {
        let bytes = read_limited(path, MAX_MANIFEST_BYTES)?;
        let document: TrustedKeysDocumentV2 =
            serde_json::from_slice(&bytes).map_err(ArtifactError::Decode)?;
        if document.schema_version != ARTIFACT_INDEX_VERSION {
            return Err(ArtifactError::UnsupportedVersion(document.schema_version));
        }
        let mut keys = BTreeMap::new();
        for (id, encoded) in document.keys {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err(ArtifactError::Trust("invalid key id".to_owned()));
            }
            let raw = BASE64
                .decode(encoded)
                .map_err(|error| ArtifactError::Trust(format!("decode key {id}: {error}")))?;
            let raw: [u8; 32] = raw.try_into().map_err(|value: Vec<u8>| {
                ArtifactError::Trust(format!("key {id} is {} bytes instead of 32", value.len()))
            })?;
            let key = VerifyingKey::from_bytes(&raw)
                .map_err(|error| ArtifactError::Trust(format!("parse key {id}: {error}")))?;
            if keys.insert(id.clone(), key).is_some() {
                return Err(ArtifactError::Trust(format!("duplicate key id {id}")));
            }
        }
        if keys.is_empty() {
            return Err(ArtifactError::Trust(
                "trusted key document contains no keys".to_owned(),
            ));
        }
        Ok(Self { keys })
    }

    fn verify(&self, manifest: &ArtifactBundleV2) -> Result<(), ArtifactError> {
        let key = self
            .keys
            .get(&manifest.signer_key_id)
            .ok_or_else(|| ArtifactError::UntrustedSigner(manifest.signer_key_id.clone()))?;
        let signature_bytes = BASE64
            .decode(&manifest.signature_ed25519)
            .map_err(|error| ArtifactError::Signature(format!("decode signature: {error}")))?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|error| ArtifactError::Signature(format!("parse signature: {error}")))?;
        let payload = canonical_payload(manifest)?;
        key.verify(&payload, &signature)
            .map_err(|error| ArtifactError::Signature(error.to_string()))
    }

    pub fn verify_detached(
        &self,
        signer_key_id: &str,
        signature_ed25519: &str,
        payload: &[u8],
    ) -> Result<(), ArtifactError> {
        let key = self
            .keys
            .get(signer_key_id)
            .ok_or_else(|| ArtifactError::UntrustedSigner(signer_key_id.to_owned()))?;
        let signature_bytes = BASE64
            .decode(signature_ed25519)
            .map_err(|error| ArtifactError::Signature(format!("decode signature: {error}")))?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|error| ArtifactError::Signature(format!("parse signature: {error}")))?;
        key.verify(payload, &signature)
            .map_err(|error| ArtifactError::Signature(error.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedBundleSetV2 {
    pub guest_manifest: ArtifactBundleV2,
    pub host_manifest: ArtifactBundleV2,
    pub artifacts: ResolvedGuestArtifactsV2,
}

/// Creates manifest-only bundle records for an explicitly enabled local direct-image workflow.
/// The normal resolver never calls this path and continues to require signed bundles.
#[allow(clippy::too_many_lines)]
pub fn prepare_direct_dev_artifact_store(
    paths: &DataPaths,
) -> Result<ArtifactSelectionV2, ArtifactError> {
    if !crate::dev::fast_artifacts_enabled() {
        return Err(ArtifactError::Manifest(
            "direct development artifacts require HD_DEV_FAST_ARTIFACTS=1".to_owned(),
        ));
    }
    let guest_root = required_env_path("HD_DEV_GUEST_BUNDLE_ROOT")?;
    let host_root = required_env_path("HD_DEV_HOST_TOOLS_ROOT")?;
    #[cfg(target_os = "macos")]
    let gfxstream_backend = macos_dev_gfxstream_backend(&host_root);
    let rootfs = std::env::var_os("HD_DEV_GUEST_ROOTFS")
        .map_or_else(|| guest_root.join("aggregate_android.img"), PathBuf::from);
    let sensor = std::env::var_os("HD_DEV_SENSOR_INJECTOR")
        .map(PathBuf::from)
        .or_else(|| {
            let path = host_root.join("hd-sensor-injector");
            path.is_file().then_some(path)
        });
    let adb = required_env_path("HD_DEV_ADB")?;
    #[cfg(windows)]
    let aapt2 = required_env_path("HD_DEV_AAPT2")?;
    #[cfg(target_os = "macos")]
    let aapt2 = std::env::var_os("HD_DEV_AAPT2").map(PathBuf::from);
    #[cfg(windows)]
    let adb_root = adb.parent().ok_or_else(|| {
        ArtifactError::Manifest(format!("ADB path has no parent: {}", adb.display()))
    })?;

    let mut guest_specs = vec![
        (
            "kernel",
            PathBuf::from("kernel"),
            guest_root.join("kernel"),
            true,
        ),
        (
            "initrd",
            PathBuf::from("initrd_android.img"),
            guest_root.join("initrd_android.img"),
            false,
        ),
        (
            "rootfs",
            PathBuf::from("aggregate_android.img"),
            rootfs,
            false,
        ),
        (
            "android_fstab",
            PathBuf::from("android_fstab.dt"),
            guest_root.join("android_fstab.dt"),
            false,
        ),
    ];
    if let Some(sensor) = sensor {
        guest_specs.push((
            "sensor-injector",
            PathBuf::from("hd-sensor-injector"),
            sensor,
            true,
        ));
    }
    #[cfg(windows)]
    let host_specs = vec![
        ("crosvm", "crosvm.exe", host_root.join("crosvm.exe"), true),
        ("adb", "adb.exe", adb.clone(), true),
        ("aapt2", "aapt2.exe", aapt2, true),
        (
            "hd-device-sim",
            "hd-device-sim.exe",
            host_root.join("hd-device-sim.exe"),
            true,
        ),
        (
            "frame-producer",
            "hd-frame-producer.exe",
            host_root.join("hd-frame-producer.exe"),
            true,
        ),
        (
            "adb-bridge",
            "hd-adb-bridge.exe",
            host_root.join("hd-adb-bridge.exe"),
            true,
        ),
        (
            "rootcanal-adapter",
            "hd-rootcanal-adapter.exe",
            host_root.join("hd-rootcanal-adapter.exe"),
            true,
        ),
        (
            "casimir-adapter",
            "hd-casimir-adapter.exe",
            host_root.join("hd-casimir-adapter.exe"),
            true,
        ),
        (
            "modem-adapter",
            "hd-modem-adapter.exe",
            host_root.join("hd-modem-adapter.exe"),
            true,
        ),
        (
            "uwb-adapter",
            "hd-uwb-adapter.exe",
            host_root.join("hd-uwb-adapter.exe"),
            true,
        ),
        (
            "gfxstream-backend",
            "libgfxstream_backend.dll",
            host_root.join("libgfxstream_backend.dll"),
            false,
        ),
        (
            "angle-egl",
            "libEGL.dll",
            host_root.join("libEGL.dll"),
            false,
        ),
        (
            "angle-glesv2",
            "libGLESv2.dll",
            host_root.join("libGLESv2.dll"),
            false,
        ),
        (
            "angle-vulkan-loader",
            "vulkan-1.dll",
            host_root.join("vulkan-1.dll"),
            false,
        ),
        (
            "crosvm-runtime-slirp",
            "libslirp-0.dll",
            host_root.join("libslirp-0.dll"),
            false,
        ),
        (
            "crosvm-runtime-audio",
            "r8Brain.dll",
            host_root.join("r8Brain.dll"),
            false,
        ),
        (
            "gnu-runtime-stdcpp",
            "libstdc++-6.dll",
            host_root.join("libstdc++-6.dll"),
            false,
        ),
        (
            "gnu-runtime-gcc",
            "libgcc_s_seh-1.dll",
            host_root.join("libgcc_s_seh-1.dll"),
            false,
        ),
        (
            "gnu-runtime-winpthread",
            "libwinpthread-1.dll",
            host_root.join("libwinpthread-1.dll"),
            false,
        ),
        (
            "adb-winapi",
            "AdbWinApi.dll",
            adb_root.join("AdbWinApi.dll"),
            false,
        ),
        (
            "adb-winusb",
            "AdbWinUsbApi.dll",
            adb_root.join("AdbWinUsbApi.dll"),
            false,
        ),
    ];
    #[cfg(target_os = "macos")]
    let mut host_specs = vec![
        ("crosvm", "crosvm", host_root.join("crosvm"), true),
        ("adb", "adb", adb.clone(), true),
        (
            "hd-device-sim",
            "hd-device-sim",
            host_root.join("hd-device-sim"),
            true,
        ),
        (
            "frame-producer",
            "hd-frame-producer",
            host_root.join("hd-frame-producer"),
            true,
        ),
        (
            "adb-bridge",
            "hd-adb-bridge",
            host_root.join("hd-adb-bridge"),
            true,
        ),
        (
            "rootcanal-adapter",
            "hd-rootcanal-adapter-disabled",
            host_root.join("hd-rootcanal-adapter-disabled"),
            true,
        ),
        (
            "casimir-adapter",
            "hd-casimir-adapter",
            host_root.join("hd-casimir-adapter"),
            true,
        ),
        (
            "modem-adapter",
            "hd-modem-adapter",
            host_root.join("hd-modem-adapter"),
            true,
        ),
        (
            "uwb-adapter",
            "hd-uwb-adapter",
            host_root.join("hd-uwb-adapter"),
            true,
        ),
        (
            "gfxstream-backend",
            "libgfxstream_backend.dylib",
            gfxstream_backend,
            false,
        ),
        (
            "crosvm-runtime-audio",
            "crosvm-runtime-audio-disabled",
            host_root.join("crosvm-runtime-audio-disabled"),
            false,
        ),
    ];
    #[cfg(target_os = "macos")]
    if let Some(aapt2) = aapt2 {
        host_specs.push(("aapt2", "aapt2", aapt2, true));
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let host_specs = Vec::<(&str, &str, PathBuf, bool)>::new();
    let guest_files = guest_specs
        .into_iter()
        .map(|(role, relative, actual, executable)| {
            dev_artifact_file(role, relative, &actual, executable)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let host_files = host_specs
        .into_iter()
        .map(|(role, relative, actual, executable)| {
            dev_artifact_file(role, PathBuf::from(relative), &actual, executable)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let guest = finish_dev_manifest(ArtifactBundleV2 {
        schema_version: ARTIFACT_INDEX_VERSION,
        digest: String::new(),
        kind: ArtifactBundleKindV2::Guest,
        platform: "android".to_owned(),
        architecture: if cfg!(target_os = "macos") {
            "arm64"
        } else {
            "x86_64"
        }
        .to_owned(),
        source_manifest_digest: hex::encode(Sha256::digest(b"hd-direct-dev-guest-v2")),
        files: guest_files,
        capabilities: vec![
            "android-15.0.0_r14".to_owned(),
            "hd-guest-profile-v2".to_owned(),
            "hd-device-bridge-v2".to_owned(),
        ],
        signer_key_id: "dev-fast-unsigned".to_owned(),
        signature_ed25519: String::new(),
    })?;
    let frame_capability = match hd_platform::platform_name() {
        "windows" => "frame-vulkan-win32-v2",
        "linux" => "frame-vulkan-dmabuf-v2",
        "macos" => "frame-metal-iosurface-v2",
        _ => return Err(ArtifactError::UnsupportedHost),
    };
    let host = finish_dev_manifest(ArtifactBundleV2 {
        schema_version: ARTIFACT_INDEX_VERSION,
        digest: String::new(),
        kind: ArtifactBundleKindV2::HostTools,
        platform: hd_platform::platform_name().to_owned(),
        architecture: hd_platform::architecture_name().to_owned(),
        source_manifest_digest: hex::encode(Sha256::digest(b"hd-direct-dev-host-v2")),
        files: host_files,
        capabilities: vec![
            "hd-host-tools-v2".to_owned(),
            "input-external-v2".to_owned(),
            "device-profile-cf-phone-v2".to_owned(),
            frame_capability.to_owned(),
            "adb-loopback-vsock-v2".to_owned(),
        ],
        signer_key_id: "dev-fast-unsigned".to_owned(),
        signature_ed25519: String::new(),
    })?;
    let store_root = paths.cache.join("direct-dev-artifacts-v2");
    write_dev_bundle(&store_root, &guest)?;
    write_dev_bundle(&store_root, &host)?;
    let trust_path = paths.root.join("trusted-keys-v2.json");
    if !trust_path.exists() {
        // A valid deterministic public key lets the trust document parse. Fast development bundle
        // resolution intentionally does not verify the unsigned manifest; production resolution does.
        let key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]).verifying_key();
        let trust = serde_json::json!({
            "schema_version": ARTIFACT_INDEX_VERSION,
            "keys": { "dev-fast-unsigned": BASE64.encode(key.as_bytes()) }
        });
        hd_platform::write_owner_only(
            &trust_path,
            &serde_json::to_vec_pretty(&trust).map_err(ArtifactError::Encode)?,
        )?;
    }
    Ok(ArtifactSelectionV2 {
        store_root,
        guest_bundle_digest: guest.digest,
        host_bundle_digest: host.digest,
    })
}

fn required_env_path(name: &'static str) -> Result<PathBuf, ArtifactError> {
    std::env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        ArtifactError::Manifest(format!(
            "{name} is required for direct development artifacts"
        ))
    })
}

#[cfg(target_os = "macos")]
fn default_macos_dev_gfxstream_backend(host_root: &Path) -> PathBuf {
    host_root
        .parent()
        .unwrap_or(host_root)
        .join("lib/libgfxstream_backend.dylib")
}

#[cfg(target_os = "macos")]
fn macos_dev_gfxstream_backend(host_root: &Path) -> PathBuf {
    std::env::var_os("HD_DEV_GFXSTREAM_BACKEND").map_or_else(
        || default_macos_dev_gfxstream_backend(host_root),
        PathBuf::from,
    )
}

fn dev_artifact_file(
    role: &str,
    relative_path: PathBuf,
    actual_path: &Path,
    executable: bool,
) -> Result<ArtifactFileV2, ArtifactError> {
    let metadata = regular_dev_file_metadata(actual_path, role)?;
    Ok(ArtifactFileV2 {
        role: role.to_owned(),
        relative_path,
        sha256: "00".repeat(32),
        size_bytes: metadata.len(),
        executable,
    })
}

fn regular_dev_file_metadata(path: &Path, role: &str) -> Result<std::fs::Metadata, ArtifactError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| ArtifactError::Io {
        operation: "inspect direct development artifact",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ArtifactError::Manifest(format!(
            "direct development role {role} is not a regular file: {}",
            path.display()
        )));
    }
    Ok(metadata)
}

fn finish_dev_manifest(mut manifest: ArtifactBundleV2) -> Result<ArtifactBundleV2, ArtifactError> {
    manifest.digest = hex::encode(Sha256::digest(canonical_payload(&manifest)?));
    Ok(manifest)
}

fn write_dev_bundle(store_root: &Path, manifest: &ArtifactBundleV2) -> Result<(), ArtifactError> {
    let root = store_root.join("bundles").join(&manifest.digest);
    hd_platform::ensure_owner_only_directory(&root)?;
    let manifest_bytes = serde_json::to_vec_pretty(manifest).map_err(ArtifactError::Encode)?;
    hd_platform::write_owner_only(&root.join(MANIFEST_FILE), &manifest_bytes)?;
    let ready = ArtifactReadyMarkerV2 {
        schema_version: ARTIFACT_INDEX_VERSION,
        bundle_digest: manifest.digest.clone(),
        manifest_sha256: hex::encode(Sha256::digest(&manifest_bytes)),
        published_at: OffsetDateTime::now_utc(),
    };
    hd_platform::write_owner_only(
        &root.join(READY_FILE),
        &serde_json::to_vec_pretty(&ready).map_err(ArtifactError::Encode)?,
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ArtifactResolver {
    trust: ArtifactTrustStore,
}

impl ArtifactResolver {
    pub fn new(trust: ArtifactTrustStore) -> Self {
        Self { trust }
    }

    pub fn resolve(
        &self,
        selection: &ArtifactSelectionV2,
    ) -> Result<ResolvedBundleSetV2, ArtifactError> {
        let guest_root = bundle_root(&selection.store_root, &selection.guest_bundle_digest);
        let host_root = bundle_root(&selection.store_root, &selection.host_bundle_digest);
        let guest = self.verify_bundle(
            &guest_root,
            &selection.guest_bundle_digest,
            ArtifactBundleKindV2::Guest,
        )?;
        let host = self.verify_bundle(
            &host_root,
            &selection.host_bundle_digest,
            ArtifactBundleKindV2::HostTools,
        )?;
        validate_platform(&host)?;
        validate_guest_architecture(&guest)?;
        let guest_files = role_map(&guest_root, &guest)?;
        let host_tools = role_map(&host_root, &host)?;
        for role in [
            "kernel",
            "initrd",
            "rootfs",
            "android_fstab",
            "sensor-injector",
        ] {
            require_role(&guest_files, role)?;
        }
        require_executable_roles(&guest, &["sensor-injector"])?;
        for role in ["crosvm", "adb", "aapt2", "hd-device-sim", "frame-producer"] {
            require_role(&host_tools, role)?;
        }
        require_windows_runtime_roles(&host_tools)?;
        for role in [
            "adb-bridge",
            "rootcanal-adapter",
            "casimir-adapter",
            "modem-adapter",
            "uwb-adapter",
        ] {
            require_role(&host_tools, role)?;
        }
        require_executable_roles(
            &host,
            &[
                "crosvm",
                "adb",
                "aapt2",
                "hd-device-sim",
                "frame-producer",
                "adb-bridge",
                "rootcanal-adapter",
                "casimir-adapter",
                "modem-adapter",
                "uwb-adapter",
            ],
        )?;
        require_capabilities(
            &guest,
            &[
                "android-15.0.0_r14",
                "hd-guest-profile-v2",
                "hd-device-bridge-v2",
            ],
        )?;
        let frame_transport = match hd_platform::platform_name() {
            "windows" => "frame-vulkan-win32-v2",
            "linux" => "frame-vulkan-dmabuf-v2",
            "macos" => "frame-metal-iosurface-v2",
            _ => return Err(ArtifactError::UnsupportedHost),
        };
        require_capabilities(
            &host,
            &[
                "hd-host-tools-v2",
                "input-external-v2",
                "device-profile-cf-phone-v2",
                frame_transport,
            ],
        )?;
        require_capabilities(&host, &["adb-loopback-vsock-v2"])?;
        Ok(ResolvedBundleSetV2 {
            artifacts: ResolvedGuestArtifactsV2 {
                guest_bundle_digest: guest.digest.clone(),
                host_bundle_digest: host.digest.clone(),
                guest_bundle_root: guest_root,
                host_bundle_root: host_root,
                kernel: guest_files["kernel"].clone(),
                initrd: guest_files["initrd"].clone(),
                rootfs: guest_files["rootfs"].clone(),
                android_fstab: guest_files["android_fstab"].clone(),
                sensor_injector: guest_files["sensor-injector"].clone(),
                system_image: guest_files.get("system_image").cloned(),
                vendor_image: guest_files.get("vendor_image").cloned(),
                host_tools,
            },
            guest_manifest: guest,
            host_manifest: host,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn resolve_fast_dev(
        &self,
        selection: &ArtifactSelectionV2,
    ) -> Result<ResolvedBundleSetV2, ArtifactError> {
        let guest_root = bundle_root(&selection.store_root, &selection.guest_bundle_digest);
        let host_root = bundle_root(&selection.store_root, &selection.host_bundle_digest);
        let guest = read_bundle_manifest_fast(
            &guest_root,
            &selection.guest_bundle_digest,
            ArtifactBundleKindV2::Guest,
        )?;
        let host = read_bundle_manifest_fast(
            &host_root,
            &selection.host_bundle_digest,
            ArtifactBundleKindV2::HostTools,
        )?;
        validate_platform(&host)?;
        validate_guest_architecture(&guest)?;
        let guest_file_root = std::env::var_os("HD_DEV_GUEST_BUNDLE_ROOT")
            .map_or_else(|| guest_root.clone(), PathBuf::from);
        let host_file_root = std::env::var_os("HD_DEV_HOST_TOOLS_ROOT")
            .map_or_else(|| host_root.clone(), PathBuf::from);
        let mut guest_files = role_map(&guest_file_root, &guest)?;
        if let Some(path) = std::env::var_os("HD_DEV_GUEST_ROOTFS") {
            guest_files.insert("rootfs".to_owned(), PathBuf::from(path));
        }
        if let Some(path) = std::env::var_os("HD_DEV_SENSOR_INJECTOR") {
            guest_files.insert("sensor-injector".to_owned(), PathBuf::from(path));
        }
        let mut host_tools = role_map(&host_file_root, &host)?;
        #[cfg(target_os = "macos")]
        {
            // Development builds stage executables in dist/macos/bin and dylibs in
            // dist/macos/lib. The manifest stays flat for publishable bundles, so
            // point the fast-development resolver at the dylib that was actually
            // validated above instead of accepting a stale copy next to crosvm.
            let gfxstream_backend = macos_dev_gfxstream_backend(&host_file_root);
            if gfxstream_backend.is_file() {
                host_tools.insert("gfxstream-backend".to_owned(), gfxstream_backend);
            }
        }
        if let Some(path) = std::env::var_os("HD_DEV_ADB") {
            let path = PathBuf::from(path);
            #[cfg(windows)]
            if let Some(parent) = path.parent() {
                host_tools.insert("adb-winapi".to_owned(), parent.join("AdbWinApi.dll"));
                host_tools.insert("adb-winusb".to_owned(), parent.join("AdbWinUsbApi.dll"));
            }
            host_tools.insert("adb".to_owned(), path);
        }
        if let Some(path) = std::env::var_os("HD_DEV_AAPT2") {
            host_tools.insert("aapt2".to_owned(), PathBuf::from(path));
        }
        require_dev_files(&guest_files, "guest")?;
        require_dev_files(&host_tools, "host tool")?;
        for role in ["kernel", "initrd", "rootfs", "android_fstab"] {
            require_role(&guest_files, role)?;
        }
        for role in ["crosvm", "adb", "hd-device-sim", "frame-producer"] {
            require_role(&host_tools, role)?;
        }
        require_windows_runtime_roles(&host_tools)?;
        for role in [
            "adb-bridge",
            "rootcanal-adapter",
            "casimir-adapter",
            "modem-adapter",
            "uwb-adapter",
        ] {
            require_role(&host_tools, role)?;
        }
        let mut executable_roles = vec![
            "crosvm",
            "adb",
            "hd-device-sim",
            "frame-producer",
            "adb-bridge",
            "rootcanal-adapter",
            "casimir-adapter",
            "modem-adapter",
            "uwb-adapter",
        ];
        if host_tools.contains_key("aapt2") {
            executable_roles.push("aapt2");
        }
        require_executable_roles(&host, &executable_roles)?;
        require_capabilities(
            &guest,
            &[
                "android-15.0.0_r14",
                "hd-guest-profile-v2",
                "hd-device-bridge-v2",
            ],
        )?;
        let frame_transport = match hd_platform::platform_name() {
            "windows" => "frame-vulkan-win32-v2",
            "linux" => "frame-vulkan-dmabuf-v2",
            "macos" => "frame-metal-iosurface-v2",
            _ => return Err(ArtifactError::UnsupportedHost),
        };
        require_capabilities(
            &host,
            &[
                "hd-host-tools-v2",
                "input-external-v2",
                "device-profile-cf-phone-v2",
                frame_transport,
            ],
        )?;
        require_capabilities(&host, &["adb-loopback-vsock-v2"])?;
        Ok(ResolvedBundleSetV2 {
            artifacts: ResolvedGuestArtifactsV2 {
                guest_bundle_digest: guest.digest.clone(),
                host_bundle_digest: host.digest.clone(),
                guest_bundle_root: guest_root,
                host_bundle_root: host_root,
                kernel: guest_files["kernel"].clone(),
                initrd: guest_files["initrd"].clone(),
                rootfs: guest_files["rootfs"].clone(),
                android_fstab: guest_files["android_fstab"].clone(),
                sensor_injector: guest_files
                    .get("sensor-injector")
                    .cloned()
                    .unwrap_or_default(),
                system_image: guest_files.get("system_image").cloned(),
                vendor_image: guest_files.get("vendor_image").cloned(),
                host_tools,
            },
            guest_manifest: guest,
            host_manifest: host,
        })
    }

    pub fn verify_bundle(
        &self,
        root: &Path,
        expected_digest: &str,
        expected_kind: ArtifactBundleKindV2,
    ) -> Result<ArtifactBundleV2, ArtifactError> {
        validate_digest(expected_digest)?;
        let manifest_path = root.join(MANIFEST_FILE);
        let ready_path = root.join(READY_FILE);
        let manifest_bytes = read_limited(&manifest_path, MAX_MANIFEST_BYTES)?;
        let ready_bytes = read_limited(&ready_path, MAX_MANIFEST_BYTES)?;
        let manifest: ArtifactBundleV2 =
            serde_json::from_slice(&manifest_bytes).map_err(ArtifactError::Decode)?;
        let ready: ArtifactReadyMarkerV2 =
            serde_json::from_slice(&ready_bytes).map_err(ArtifactError::Decode)?;
        if manifest.schema_version != ARTIFACT_INDEX_VERSION
            || ready.schema_version != ARTIFACT_INDEX_VERSION
        {
            return Err(ArtifactError::UnsupportedVersion(
                manifest.schema_version.min(ready.schema_version),
            ));
        }
        if manifest.kind != expected_kind {
            return Err(ArtifactError::WrongKind {
                expected: expected_kind,
                actual: manifest.kind,
            });
        }
        let computed_digest = hex::encode(Sha256::digest(canonical_payload(&manifest)?));
        if manifest.digest != expected_digest || manifest.digest != computed_digest {
            return Err(ArtifactError::DigestMismatch {
                label: "bundle manifest".to_owned(),
                expected: expected_digest.to_owned(),
                actual: computed_digest,
            });
        }
        if ready.bundle_digest != manifest.digest {
            return Err(ArtifactError::DigestMismatch {
                label: "READY bundle".to_owned(),
                expected: manifest.digest.clone(),
                actual: ready.bundle_digest,
            });
        }
        let actual_manifest_sha = hex::encode(Sha256::digest(&manifest_bytes));
        if ready.manifest_sha256 != actual_manifest_sha {
            return Err(ArtifactError::DigestMismatch {
                label: "READY manifest".to_owned(),
                expected: ready.manifest_sha256,
                actual: actual_manifest_sha,
            });
        }
        self.trust.verify(&manifest)?;
        validate_bundle_contents(root, &manifest)?;
        Ok(manifest)
    }
}

fn read_bundle_manifest_fast(
    root: &Path,
    expected_digest: &str,
    expected_kind: ArtifactBundleKindV2,
) -> Result<ArtifactBundleV2, ArtifactError> {
    validate_digest(expected_digest)?;
    let manifest_path = root.join(MANIFEST_FILE);
    let ready_path = root.join(READY_FILE);
    let manifest_bytes = read_limited(&manifest_path, MAX_MANIFEST_BYTES)?;
    let ready_bytes = read_limited(&ready_path, MAX_MANIFEST_BYTES)?;
    let manifest: ArtifactBundleV2 =
        serde_json::from_slice(&manifest_bytes).map_err(ArtifactError::Decode)?;
    let ready: ArtifactReadyMarkerV2 =
        serde_json::from_slice(&ready_bytes).map_err(ArtifactError::Decode)?;
    if manifest.schema_version != ARTIFACT_INDEX_VERSION
        || ready.schema_version != ARTIFACT_INDEX_VERSION
    {
        return Err(ArtifactError::UnsupportedVersion(
            manifest.schema_version.min(ready.schema_version),
        ));
    }
    if manifest.kind != expected_kind {
        return Err(ArtifactError::WrongKind {
            expected: expected_kind,
            actual: manifest.kind,
        });
    }
    let computed_digest = hex::encode(Sha256::digest(canonical_payload(&manifest)?));
    if manifest.digest != expected_digest || manifest.digest != computed_digest {
        return Err(ArtifactError::DigestMismatch {
            label: "bundle manifest".to_owned(),
            expected: expected_digest.to_owned(),
            actual: computed_digest,
        });
    }
    if ready.bundle_digest != manifest.digest {
        return Err(ArtifactError::DigestMismatch {
            label: "READY bundle".to_owned(),
            expected: manifest.digest.clone(),
            actual: ready.bundle_digest,
        });
    }
    validate_bundle_manifest_fast(&manifest)?;
    Ok(manifest)
}

fn require_windows_runtime_roles(
    host_tools: &BTreeMap<String, PathBuf>,
) -> Result<(), ArtifactError> {
    if hd_platform::platform_name() != "windows" {
        return Ok(());
    }
    for role in [
        "gfxstream-backend",
        "angle-egl",
        "angle-glesv2",
        "angle-vulkan-loader",
        "crosvm-runtime-slirp",
        "crosvm-runtime-audio",
        "gnu-runtime-stdcpp",
        "gnu-runtime-gcc",
        "gnu-runtime-winpthread",
        "adb-winapi",
        "adb-winusb",
    ] {
        require_role(host_tools, role)?;
    }
    Ok(())
}

fn validate_bundle_contents(root: &Path, manifest: &ArtifactBundleV2) -> Result<(), ArtifactError> {
    validate_bundle_manifest_fast(manifest)?;
    for file in &manifest.files {
        verify_file(root, file)?;
    }
    Ok(())
}

fn validate_bundle_manifest_fast(manifest: &ArtifactBundleV2) -> Result<(), ArtifactError> {
    validate_digest(&manifest.source_manifest_digest)?;
    if manifest.files.is_empty() {
        return Err(ArtifactError::Manifest("bundle has no files".to_owned()));
    }
    let mut roles = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        validate_relative_path(&file.relative_path)?;
        if file.role.is_empty()
            || file.role.len() > 128
            || !file
                .role
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ArtifactError::Manifest("invalid file role".to_owned()));
        }
        if !roles.insert(file.role.clone()) {
            return Err(ArtifactError::Manifest(format!(
                "duplicate file role {}",
                file.role
            )));
        }
        if !paths.insert(file.relative_path.clone()) {
            return Err(ArtifactError::Manifest(format!(
                "duplicate artifact path {}",
                file.relative_path.display()
            )));
        }
        if file.size_bytes == 0 {
            return Err(ArtifactError::Manifest(format!(
                "artifact {} is empty",
                file.relative_path.display()
            )));
        }
        validate_digest(&file.sha256)?;
    }
    let mut capabilities = BTreeSet::new();
    if manifest.capabilities.is_empty() || manifest.capabilities.len() > 128 {
        return Err(ArtifactError::Manifest(
            "bundle capabilities must contain 1..=128 entries".to_owned(),
        ));
    }
    for capability in &manifest.capabilities {
        if capability.is_empty()
            || capability.len() > 128
            || !capability
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !capabilities.insert(capability)
        {
            return Err(ArtifactError::Manifest(format!(
                "invalid or duplicate bundle capability {capability}"
            )));
        }
    }
    Ok(())
}

pub fn artifact_diagnostics(
    selection: Option<&ArtifactSelectionV2>,
    trust_path: &Path,
) -> Vec<DiagnosticCheckV2> {
    let mut checks = Vec::new();
    checks.push(path_check("artifact.trust_store", trust_path));
    if let Some(selection) = selection {
        checks.push(path_check(
            "artifact.guest_ready",
            &bundle_root(&selection.store_root, &selection.guest_bundle_digest).join(READY_FILE),
        ));
        checks.push(path_check(
            "artifact.host_ready",
            &bundle_root(&selection.store_root, &selection.host_bundle_digest).join(READY_FILE),
        ));
    } else {
        checks.push(DiagnosticCheckV2 {
            id: "artifact.selection".to_owned(),
            status: DiagnosticStatusV2::Blocked,
            detail: "no signed artifact bundle is selected".to_owned(),
            fields: BTreeMap::new(),
        });
    }
    checks
}

pub fn canonical_payload(manifest: &ArtifactBundleV2) -> Result<Vec<u8>, ArtifactError> {
    let mut payload = manifest.clone();
    payload.digest.clear();
    payload.signature_ed25519.clear();
    serde_json::to_vec(&payload).map_err(ArtifactError::Encode)
}

fn bundle_root(store_root: &Path, digest: &str) -> PathBuf {
    store_root.join("bundles").join(digest)
}

fn role_map(
    root: &Path,
    manifest: &ArtifactBundleV2,
) -> Result<BTreeMap<String, PathBuf>, ArtifactError> {
    manifest
        .files
        .iter()
        .map(|file| {
            validate_relative_path(&file.relative_path)?;
            Ok((file.role.clone(), root.join(&file.relative_path)))
        })
        .collect()
}

fn require_role(files: &BTreeMap<String, PathBuf>, role: &str) -> Result<(), ArtifactError> {
    if files.contains_key(role) {
        Ok(())
    } else {
        Err(ArtifactError::MissingRole(role.to_owned()))
    }
}

fn require_dev_files(
    files: &BTreeMap<String, PathBuf>,
    bundle_label: &str,
) -> Result<(), ArtifactError> {
    for (role, path) in files {
        let metadata = std::fs::symlink_metadata(path).map_err(|source| ArtifactError::Io {
            operation: "inspect fast-development artifact",
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(ArtifactError::Manifest(format!(
                "fast-development {bundle_label} role {role} is not a regular file: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn require_executable_roles(
    manifest: &ArtifactBundleV2,
    roles: &[&str],
) -> Result<(), ArtifactError> {
    for role in roles {
        let executable = manifest
            .files
            .iter()
            .find(|file| file.role == *role)
            .is_some_and(|file| file.executable);
        if !executable {
            return Err(ArtifactError::Manifest(format!(
                "host tool role {role} is not declared executable"
            )));
        }
    }
    Ok(())
}

fn require_capabilities(
    manifest: &ArtifactBundleV2,
    required: &[&str],
) -> Result<(), ArtifactError> {
    for capability in required {
        if !manifest
            .capabilities
            .iter()
            .any(|value| value == capability)
        {
            return Err(ArtifactError::MissingCapability((*capability).to_owned()));
        }
    }
    Ok(())
}

fn validate_platform(manifest: &ArtifactBundleV2) -> Result<(), ArtifactError> {
    let expected_platform = hd_platform::platform_name();
    let expected_architecture = hd_platform::architecture_name();
    if manifest.platform != expected_platform || manifest.architecture != expected_architecture {
        return Err(ArtifactError::HostMismatch {
            expected: format!("{expected_platform}/{expected_architecture}"),
            actual: format!("{}/{}", manifest.platform, manifest.architecture),
        });
    }
    Ok(())
}

fn validate_guest_architecture(manifest: &ArtifactBundleV2) -> Result<(), ArtifactError> {
    if manifest.platform != "android" {
        return Err(ArtifactError::GuestMismatch {
            expected: "android".to_owned(),
            actual: manifest.platform.clone(),
        });
    }
    let expected_architecture = if cfg!(target_os = "macos") {
        "arm64"
    } else {
        "x86_64"
    };
    if manifest.architecture != expected_architecture {
        return Err(ArtifactError::GuestMismatch {
            expected: expected_architecture.to_owned(),
            actual: manifest.architecture.clone(),
        });
    }
    Ok(())
}

fn verify_file(root: &Path, file: &hd_core::ArtifactFileV2) -> Result<(), ArtifactError> {
    let path = root.join(&file.relative_path);
    let metadata = std::fs::symlink_metadata(&path).map_err(|source| ArtifactError::Io {
        operation: "read artifact metadata",
        path: path.clone(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ArtifactError::Manifest(format!(
            "artifact {} is not a regular non-symlink file",
            path.display()
        )));
    }
    if metadata.len() != file.size_bytes {
        return Err(ArtifactError::SizeMismatch {
            path,
            expected: file.size_bytes,
            actual: metadata.len(),
        });
    }
    let actual = sha256_file(&path)?;
    if actual != file.sha256 {
        return Err(ArtifactError::DigestMismatch {
            label: path.display().to_string(),
            expected: file.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String, ArtifactError> {
    let file = hd_platform::open_regular_read_nofollow(path)?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024].into_boxed_slice();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| ArtifactError::Io {
                operation: "hash artifact",
                path: path.to_owned(),
                source,
            })?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, ArtifactError> {
    hd_platform::read_regular_nofollow_limited(path, limit).map_err(ArtifactError::Platform)
}

fn validate_relative_path(path: &Path) -> Result<(), ArtifactError> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ArtifactError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), ArtifactError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ArtifactError::Manifest(
            "digest must be a lowercase SHA-256 value".to_owned(),
        ));
    }
    Ok(())
}

fn path_check(id: &str, path: &Path) -> DiagnosticCheckV2 {
    let exists = path.is_file();
    DiagnosticCheckV2 {
        id: id.to_owned(),
        status: if exists {
            DiagnosticStatusV2::Pass
        } else {
            DiagnosticStatusV2::Blocked
        },
        detail: if exists {
            "present".to_owned()
        } else {
            "missing".to_owned()
        },
        fields: BTreeMap::from([("path".to_owned(), path.display().to_string())]),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedKeysDocumentV2 {
    schema_version: u32,
    keys: BTreeMap<String, String>,
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("unsupported artifact schema version {0}")]
    UnsupportedVersion(u32),
    #[error("artifact trust configuration is invalid: {0}")]
    Trust(String),
    #[error("artifact signer is not trusted: {0}")]
    UntrustedSigner(String),
    #[error("artifact signature verification failed: {0}")]
    Signature(String),
    #[error("artifact manifest is invalid: {0}")]
    Manifest(String),
    #[error("artifact bundle has kind {actual:?}, expected {expected:?}")]
    WrongKind {
        expected: ArtifactBundleKindV2,
        actual: ArtifactBundleKindV2,
    },
    #[error("artifact digest mismatch for {label}: expected {expected}, actual {actual}")]
    DigestMismatch {
        label: String,
        expected: String,
        actual: String,
    },
    #[error("artifact size mismatch for {path}: expected {expected}, actual {actual}")]
    SizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("artifact path is unsafe: {0}")]
    UnsafePath(PathBuf),
    #[error("artifact bundle is missing required role {0}")]
    MissingRole(String),
    #[error("artifact bundle is missing required capability {0}")]
    MissingCapability(String),
    #[error("the current host platform is unsupported")]
    UnsupportedHost,
    #[error("host bundle mismatch: expected {expected}, actual {actual}")]
    HostMismatch { expected: String, actual: String },
    #[error("guest bundle mismatch: expected {expected}, actual {actual}")]
    GuestMismatch { expected: String, actual: String },
    #[error("failed to decode artifact document: {0}")]
    Decode(serde_json::Error),
    #[error("failed to encode artifact signing payload: {0}")]
    Encode(serde_json::Error),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Platform(#[from] hd_platform::PlatformError),
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::default_macos_dev_gfxstream_backend;
    use std::path::Path;

    #[test]
    fn macos_fast_dev_gfxstream_defaults_to_the_sibling_lib_directory() {
        assert_eq!(
            default_macos_dev_gfxstream_backend(Path::new("/workspace/out/dist/macos/bin")),
            Path::new("/workspace/out/dist/macos/lib/libgfxstream_backend.dylib")
        );
    }
}

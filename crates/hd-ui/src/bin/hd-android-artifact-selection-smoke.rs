use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use hd_core::{CapabilityStatusV2, HostCapabilitiesV2};
use hd_ui::ui_contract::{
    direct_android_artifact_candidates, resolve_direct_android_artifact_root,
};
use serde_json::json;

fn main() -> Result<()> {
    assert_instance_aware_device_gate()?;

    let temporary = tempfile::tempdir()?;
    let aosp_fstab = temporary.path().join("aosp-fstab.dt");
    fs::write(&aosp_fstab, VALID_AOSP_ALTERNATIVE_FSTAB)?;
    hd_runtime::validate_android_fstab(&aosp_fstab)?;

    let unexpected_fstab = temporary.path().join("unexpected-fstab.dt");
    fs::write(&unexpected_fstab, UNEXPECTED_ALTERNATIVE_FSTAB)?;
    ensure!(
        hd_runtime::validate_android_fstab(&unexpected_fstab).is_err(),
        "an arbitrary filesystem alternative bypassed the Android fstab contract"
    );

    let mixed_encryption_fstab = temporary.path().join("mixed-encryption-fstab.dt");
    fs::write(&mixed_encryption_fstab, MIXED_ENCRYPTION_FSTAB)?;
    ensure!(
        hd_runtime::validate_android_fstab(&mixed_encryption_fstab).is_err(),
        "mixed encrypted and unencrypted userdata alternatives bypassed the Android fstab contract"
    );

    let root = Path::new("artifact-root");
    let common = vec![root.to_owned(), root.join("direct-linux")];

    let x86 = direct_android_artifact_candidates(root, "x86_64");
    ensure!(
        x86 == [
            common.clone(),
            vec![root.join("products/android/vsoc_x86_64/direct-linux")],
        ]
        .concat(),
        "x86_64 Host did not select only the x86_64 Android product"
    );
    ensure!(
        !contains_product(&x86, "vsoc_arm64") && !contains_product(&x86, "vsoc_arm64_only"),
        "x86_64 Host retained a cross-architecture ARM fallback"
    );

    let arm64 = direct_android_artifact_candidates(root, "arm64");
    ensure!(
        arm64
            == [
                common.clone(),
                vec![
                    root.join("products/android/vsoc_arm64_only/direct-linux"),
                    root.join("products/android/vsoc_arm64/direct-linux"),
                ],
            ]
            .concat(),
        "arm64 Host did not prefer arm64-only then arm64 Android products"
    );
    ensure!(
        !contains_product(&arm64, "vsoc_x86_64"),
        "arm64 Host retained a cross-architecture x86_64 fallback"
    );

    ensure!(
        direct_android_artifact_candidates(root, "unsupported") == common,
        "unsupported Host architecture guessed a product architecture"
    );

    let supplied_root = std::env::args_os().nth(1).map(PathBuf::from);
    let selected_supplied_root = validate_supplied_root(supplied_root.as_deref())?;

    println!(
        "{}",
        serde_json::to_string(&json!({
            "gate": "android-artifact-selection-smoke",
            "status": "pass",
            "x86_64_product": "vsoc_x86_64",
            "arm64_products": ["vsoc_arm64_only", "vsoc_arm64"],
            "cross_architecture_fallback": false,
            "aosp_fstab_alternatives": "ext4|erofs and ext4|f2fs accepted",
            "arbitrary_fstab_alternatives_rejected": true,
            "mixed_data_encryption_rejected": true,
            "aosp_compact_aggregate_validated": selected_supplied_root.is_some(),
            "disabled_optional_device_not_launch_blocker": true,
            "required_device_profile_remains_launch_gate": true,
            "supplied_root_checked": supplied_root.is_some(),
            "selected_supplied_root": selected_supplied_root
        }))?
    );
    Ok(())
}

fn validate_supplied_root(root: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(root) = root else {
        return Ok(None);
    };
    let selected = resolve_direct_android_artifact_root(root, hd_platform::architecture_name())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "supplied artifact root has no complete product for this Host architecture"
            )
        })?;
    let expected = match hd_platform::architecture_name() {
        "x86_64" => "vsoc_x86_64",
        "arm64" => "vsoc_arm64_only",
        architecture => anyhow::bail!("unsupported Host architecture: {architecture}"),
    };
    ensure!(
        contains_product(std::slice::from_ref(&selected), expected),
        "supplied artifact root resolved to the wrong architecture: {}",
        selected.display()
    );
    hd_runtime::validate_android_fstab(&selected.join("android_fstab.dt"))?;
    let aggregate = selected.join("aggregate_android.img");
    if aggregate.is_file() {
        hd_runtime::validate_android_aggregate(&aggregate)?;
    }
    Ok(Some(selected))
}

fn assert_instance_aware_device_gate() -> Result<()> {
    let mut instance_aware_start: HostCapabilitiesV2 = serde_json::from_value(json!({
        "schema_version": 2,
        "generated_at": "1970-01-01T00:00:00Z",
        "platform": "windows",
        "architecture": "x86_64",
        "fingerprint": "instance-aware-device-contract",
        "verified": false,
        "certified": false,
        "development_bypass": true,
        "probes": [{
            "id": "device.profile",
            "status": "supported",
            "required": true,
            "detail": "all enabled backends are available",
            "properties": {}
        }],
        "devices": {
            "profile": "phone",
            "devices": [{
                "id": "sensors",
                "backend": "simulated",
                "available": false,
                "boundary": "optional injector is absent",
                "features": []
            }]
        }
    }))?;
    ensure!(
        instance_aware_start.can_start(),
        "an unavailable but disabled optional device bypassed the instance-aware profile probe"
    );
    instance_aware_start.probes[0].status = CapabilityStatusV2::Blocked;
    ensure!(
        !instance_aware_start.can_start(),
        "a required blocked device profile allowed launch"
    );
    Ok(())
}

const VALID_AOSP_ALTERNATIVE_FSTAB: &str = r"
system /system erofs ro wait,logical,first_stage_mount
system /system ext4 ro wait,logical,first_stage_mount
/dev/block/by-name/userdata /data f2fs nodev,nosuid,inlinecrypt latemount,wait,keydirectory=/metadata/vold/metadata_encryption
/dev/block/by-name/userdata /data ext4 nodev,nosuid,inlinecrypt latemount,wait,keydirectory=/metadata/vold/metadata_encryption
/dev/block/by-name/metadata /metadata ext4 nodev,nosuid wait,first_stage_mount
";

const UNEXPECTED_ALTERNATIVE_FSTAB: &str = r"
system /system erofs ro wait,logical,first_stage_mount
system /system squashfs ro wait,logical,first_stage_mount
/dev/block/by-name/userdata /data ext4 nodev,nosuid,inlinecrypt latemount,wait,keydirectory=/metadata/vold/metadata_encryption
/dev/block/by-name/metadata /metadata ext4 nodev,nosuid wait,first_stage_mount
";

const MIXED_ENCRYPTION_FSTAB: &str = r"
system /system ext4 ro wait,logical,first_stage_mount
/dev/block/by-name/userdata /data f2fs nodev,nosuid,inlinecrypt latemount,wait,keydirectory=/metadata/vold/metadata_encryption
/dev/block/by-name/userdata /data ext4 nodev,nosuid first_stage_mount,wait
/dev/block/by-name/metadata /metadata ext4 nodev,nosuid wait,first_stage_mount
";

fn contains_product(paths: &[PathBuf], product: &str) -> bool {
    paths.iter().any(|path| {
        path.components()
            .any(|component| component.as_os_str() == product)
    })
}

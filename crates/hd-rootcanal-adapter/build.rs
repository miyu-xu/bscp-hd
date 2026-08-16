use std::path::PathBuf;

fn main() {
    let windows = std::env::var("CARGO_CFG_WINDOWS").is_ok();
    let macos = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|value| value == "macos");
    if !windows && !macos {
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../packages/modules/Bluetooth/tools/rootcanal");
    let sources = [
        "lib/hci/address.cc",
        "model/controller/acl_connection.cc",
        "model/controller/acl_connection_handler.cc",
        "model/controller/controller_properties.cc",
        "model/controller/dual_mode_controller.cc",
        "model/controller/le_advertiser.cc",
        "model/controller/link_layer_controller.cc",
        "model/controller/sco_connection.cc",
        "model/controller/vendor_commands/le_apcf.cc",
        "model/devices/device.cc",
    ];
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .define("ROOTCANAL_STANDALONE_WINDOWS", None)
        .include(root.join("include"))
        .include(&root)
        .include(root.join("windows/generated"))
        .flag_if_supported("-fno-function-sections")
        .flag_if_supported("-fno-data-sections")
        .warnings(false);
    if windows {
        build
            .file(root.join("windows/crypto_windows.cc"))
            .file(root.join("windows/ffi_windows.cc"));
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            build.flag("/bigobj").flag("/EHsc");
        } else {
            build.flag("-Wa,-mbig-obj");
        }
    } else {
        build
            .file(root.join("macos/crypto_macos.cc"))
            .file(root.join("macos/ffi_macos.cc"));
    }
    for source in sources {
        let path = root.join(source);
        println!("cargo:rerun-if-changed={}", path.display());
        build.file(path);
    }
    for generated in [
        "windows/crypto_windows.cc",
        "windows/ffi_windows.cc",
        "macos/crypto_macos.cc",
        "macos/ffi_macos.cc",
        "windows/generated/packets/hci_packets.h",
        "windows/generated/packets/link_layer_packets.h",
        "windows/generated/packet_runtime.h",
        "rust/hci_packets.rs",
        "rust/lmp_packets.rs",
        "rust/llcp_packets.rs",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(generated).display());
    }
    build.compile("rootcanal_windows");
    if windows {
        println!("cargo:rustc-link-lib=bcrypt");
    }
}

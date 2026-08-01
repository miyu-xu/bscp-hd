fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("src/native_display_host_macos.m")
            .flag("-fblocks")
            .flag("-fobjc-arc")
            .compile("hd_native_display_host_macos");
        println!("cargo:rerun-if-changed=src/native_display_host_macos.m");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=QuartzCore");
        println!("cargo:rustc-link-lib=framework=UniformTypeIdentifiers");
    }
}

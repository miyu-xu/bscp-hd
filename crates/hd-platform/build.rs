#[cfg(target_os = "macos")]
fn main() {
    cc::Build::new()
        .file("src/native_display_host_macos.m")
        .flag("-fblocks")
        .flag("-fobjc-arc")
        .compile("hd_native_display_host_macos");
    cc::Build::new()
        .file("src/native_digest_macos.c")
        .compile("hd_native_digest_macos");
    println!("cargo:rerun-if-changed=src/native_display_host_macos.m");
    println!("cargo:rerun-if-changed=src/native_digest_macos.c");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=QuartzCore");
    println!("cargo:rustc-link-lib=framework=UniformTypeIdentifiers");
}

#[cfg(not(target_os = "macos"))]
fn main() {}

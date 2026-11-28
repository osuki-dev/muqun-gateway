fn main() {
    println!("cargo:rerun-if-changed=src/macos.m");
    #[cfg(target_os = "macos")]
    {
        cc::Build::new()
            .file("src/macos.m")
            .flag("-fobjc-arc")
            .flag("-Wno-deprecated-declarations")
            .compile("muqun_webview_macos");
        println!("cargo:rustc-link-lib=framework=Cocoa");
        println!("cargo:rustc-link-lib=framework=WebKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=QuartzCore");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
    }
}

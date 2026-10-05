fn main() {
    // Embed the app icon and version info into the Windows executable.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Kairo");
        res.set("FileDescription", "Kairo Kalender");
        res.set("CompanyName", "Kairo");
        res.compile().expect("failed to compile Windows resources");
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}

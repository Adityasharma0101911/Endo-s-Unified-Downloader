// The Windows program's icon (assets/logo.ico) and the name Explorer and Task Manager show for it.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/logo.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/logo.ico");
        // Otherwise the package's name, hyperfetch-cli.
        res.set("ProductName", "Endo's Unified Downloader").set("FileDescription", "Endo's Unified Downloader CLI");
        res.compile_for("Endos-Unified-Downloader-CLI").expect("the Windows resources compile");
    }
}

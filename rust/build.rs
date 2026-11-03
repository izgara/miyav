fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MIYAV_VERSION");
    // The exe's icon. A machine without windres only loses the icon.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let icon = std::path::Path::new(&dir).parent().unwrap().join("ui").join("app.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon.to_str().unwrap());
        if let Err(e) = res.compile() {
            println!("cargo:warning=no icon: {e}");
        }
    }
}

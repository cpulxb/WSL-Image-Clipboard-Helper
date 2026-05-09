fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/wsl_clipboard.ico");
    resource.set_manifest_file("app.manifest");
    resource.set("FileDescription", "WSL Image Clipboard Helper");
    resource.set("ProductName", "WSL Image Clipboard Helper");
    resource.set("OriginalFilename", "wsl_clipboard.exe");

    if let Err(error) = resource.compile() {
        panic!("failed to compile Windows resources: {error}");
    }
}

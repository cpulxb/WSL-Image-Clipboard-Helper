fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // 资源编译依赖 Windows 上的 rc 工具链；
    // 非 Windows 宿主跳过图标/manifest 嵌入，保证交叉 cargo check/clippy 可用
    if std::env::var("HOST")
        .map(|h| !h.contains("windows"))
        .unwrap_or(true)
    {
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

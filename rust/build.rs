fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=assets/wsl_clipboard.ico");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-env-changed=RC_PATH");

    // 非 Windows 宿主（如 WSL 里 cargo xwin 交叉编译）没有 rc.exe，改用 llvm-rc；
    // 找不到就跳过图标/manifest 嵌入（保证交叉 cargo check/clippy 可用），但要明确告警，
    // 否则编出来的 exe 托盘和文件图标都会变成系统默认图标
    let host_is_windows = std::env::var("HOST")
        .map(|h| h.contains("windows"))
        .unwrap_or(false);
    if !host_is_windows && std::env::var_os("RC_PATH").is_none() {
        match find_llvm_rc() {
            Some(rc) => std::env::set_var("RC_PATH", rc),
            None => {
                println!(
                    "cargo:warning=未找到 llvm-rc，exe 将不含图标和 manifest（可安装 llvm 或设置 RC_PATH）"
                );
                return;
            }
        }
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

/// 在 PATH 里找 llvm-rc；发行版常只装带版本号的 llvm-rc-NN，取版本最高的
fn find_llvm_rc() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let version = match name.strip_prefix("llvm-rc") {
                Some("") => u32::MAX,
                Some(v) => match v.strip_prefix('-').and_then(|v| v.parse().ok()) {
                    Some(v) => v,
                    None => continue,
                },
                None => continue,
            };
            if best.as_ref().map_or(true, |(b, _)| version > *b) {
                best = Some((version, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

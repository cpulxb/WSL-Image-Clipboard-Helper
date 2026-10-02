#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

mod cleanup;
mod clipboard;
mod config;
mod foreground;
mod hotkey;
mod image_saver;
mod paste;
mod remote;
mod remote_transport;
mod ssh_auth;
mod ssh_clients;
mod tray;

use clipboard::ClipboardManager;
use config::{ImeProtection, PathStyle, RuntimeMode};
use remote::{RemoteMode, RemoteUploader, SshSession};
use tray::TrayCommand;

/// 应用运行时状态（可被托盘命令修改）
struct AppState {
    runtime_mode: RuntimeMode,
    path_style: PathStyle,
    ime_protection: ImeProtection,
    /// 远程粘贴模式（issue #11）
    remote: RemoteMode,
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(code) = ssh_auth::handle_askpass() {
        std::process::exit(code);
    }
    // 显式诊断启动才落盘；普通启动不记录日志，也不改变用户配置。
    if std::env::args().any(|arg| arg == "--diagnostics") {
        let exe = std::env::current_exe()?;
        let path = exe.with_file_name(format!(
            "wsl_clipboard-diagnostic-{}.log",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init();
        info!(exe = %exe.display(), pid = std::process::id(), "诊断模式启动");
    } else {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();
    }

    // 重复双击 exe 时不再多开：后开的实例抢不到热键，只会在托盘里多出一个没用的图标
    let Some(_instance_guard) = acquire_single_instance() else {
        info!("已有实例在运行，退出");
        notify_already_running();
        return Ok(());
    };

    info!(
        "WSL Clipboard Helper v{} (Rust) 启动中...",
        env!("CARGO_PKG_VERSION")
    );

    // 加载配置
    let app_config = config::AppConfig::load().unwrap_or_default();
    info!(
        "加载配置: 热键={}, 模式={:?}, 输入法保护={}, 远程粘贴={}",
        app_config.hotkey,
        app_config.runtime_mode,
        app_config.ime_protection.as_str(),
        if app_config.remote_paste {
            "自动"
        } else {
            "关闭"
        }
    );

    // 确定临时目录
    let temp_dir = cleanup::temp_dir_from_current_exe()?;

    if !temp_dir.exists() {
        std::fs::create_dir_all(&temp_dir).context("创建临时目录失败")?;
    }

    info!("临时目录: {}", temp_dir.display());

    // 注意（issue #10）：这里**不再**预加载英文键盘布局。
    // 旧实现在启动时无条件调用 LoadKeyboardLayoutW("00000409", KLF_ACTIVATE)，
    // 会把英文布局登记进系统输入法列表，导致用户 Win+Space 里凭空多出
    // `ENG / English (United States)`。现在英文布局只在确实需要时惰性解析，
    // 且默认只复用系统已有布局，详见 config::ImeProtection。

    // 创建剪贴板管理器
    let clipboard_manager = Arc::new(ClipboardManager::new(temp_dir.clone()));

    // 远程上传器（issue #11）
    let remote_uploader = Arc::new(RemoteUploader::new());

    // 运行时状态
    let state = Arc::new(Mutex::new(AppState {
        runtime_mode: app_config.runtime_mode.clone(),
        path_style: app_config.path_style,
        ime_protection: app_config.ime_protection,
        remote: RemoteMode::from_config(app_config.remote_paste),
    }));

    // 启动托盘（含热键管理器）
    let std_tray_rx = tray::TrayController::start(app_config, temp_dir.clone())?;

    // 将 std mpsc 桥接到 tokio mpsc，以便在 select! 中使用
    let (tray_tx_bridge, mut tray_rx) = mpsc::channel::<TrayCommand>(32);
    tokio::task::spawn_blocking(move || loop {
        match std_tray_rx.recv() {
            Ok(cmd) => {
                if tray_tx_bridge.blocking_send(cmd).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    // 启动热键桥接
    let mut hotkey_rx = hotkey::start_hotkey_bridge();

    // 定时清理任务
    let temp_dir_for_cleanup = temp_dir.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(7200));
        loop {
            interval.tick().await;
            if let Err(e) = cleanup::cleanup_old_files(&temp_dir_for_cleanup) {
                warn!("清理临时文件失败: {}", e);
            }
        }
    });

    info!("WSL Clipboard Helper 已启动");

    let mut paste_task: Option<JoinHandle<Result<()>>> = None;
    // 主事件循环
    loop {
        tokio::select! {
            // 热键触发
            Some(_hotkey_id) = hotkey_rx.recv() => {
                // Keep the tray responsive during authentication, without queuing dialogs.
                if paste_task.is_some() { continue; }
                let (mode, path_style, ime_protection, remote_mode) = {
                    let s = state.lock().await;
                    (s.runtime_mode.clone(), s.path_style, s.ime_protection, s.remote.clone())
                };
                // 枚举 ssh 进程要走进程快照 / 起 wsl.exe（约 0.2 秒），再探测前台 tab，放到阻塞线程上
                // 与读剪贴板、落盘并行，只在真正要决定粘什么路径时才等它
                info!(remote_mode = %remote_mode.display_name(), "收到图片粘贴热键");
                let session = tokio::task::spawn_blocking(move || remote_mode.resolve());
                let clipboard = clipboard_manager.clone();
                let uploader = remote_uploader.clone();
                paste_task = Some(tokio::spawn(async move {
                    handle_paste(&clipboard, &uploader, &mode, path_style, ime_protection, session).await
                }));
            }
            result = async {
                match &mut paste_task {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            }, if paste_task.is_some() => {
                paste_task = None;
                match result {
                    Ok(Ok(())) => {},
                    Ok(Err(e)) => error!("粘贴处理失败: {}", e),
                    Err(e) => error!("粘贴任务已中止: {}", e),
                }
            }
            // 托盘命令
            Some(cmd) = tray_rx.recv() => {
                match cmd {
                    TrayCommand::SwitchHotkey(ht) => {
                        info!("主循环: 热键已切换为 {}", ht.display_name());
                    }
                    TrayCommand::SwitchMode(mode) => {
                        info!("主循环: 模式已切换为 {:?}", mode);
                        let mut s = state.lock().await;
                        s.runtime_mode = mode;
                    }
                    TrayCommand::SwitchPathStyle(style) => {
                        info!("主循环: 路径格式已切换为 {}", style.display_name());
                        let mut s = state.lock().await;
                        s.path_style = style;
                    }
                    TrayCommand::SwitchImeProtection(policy) => {
                        info!("主循环: 输入法保护已切换为 {}", policy.display_name());
                        let mut s = state.lock().await;
                        s.ime_protection = policy;
                    }
                    TrayCommand::SwitchRemoteMode(mode) => {
                        info!("主循环: 远程粘贴已切换为 {}", mode.display_name());
                        let mut s = state.lock().await;
                        s.remote = mode;
                    }
                    TrayCommand::OpenFolder => {
                        if let Err(e) = tray::open_temp_folder() {
                            error!("打开文件夹失败: {}", e);
                        }
                    }
                    TrayCommand::Exit => {
                        info!("收到退出命令");
                        break;
                    }
                }
            }
            else => {
                // 所有通道关闭
                break;
            }
        }
    }

    if let Some(task) = paste_task {
        task.abort();
        let _ = task.await; // Drop authentication guards before cleaning up channels.
    }
    // 退出前清理 temp 目录下的所有 PNG 文件
    if let Err(e) = cleanup::cleanup_temp_png(&temp_dir) {
        warn!("退出清理临时文件失败: {}", e);
    }

    // 本会话上传到远程的临时截图也一并删掉（issue #11）
    remote_uploader.cleanup_on_exit().await;

    // 若本进程曾惰性加载过英文键盘布局（ime_protection = "layout-force"），
    // 退出时卸载掉，不在用户的输入法切换列表里留下残留（issue #10）
    paste::unload_self_loaded_layout();

    info!("WSL Clipboard Helper 已退出");
    std::process::exit(0);
}

/// 单实例保护：持有一个会话内的命名 mutex，进程退出时由系统释放。
/// 返回 None 表示已有实例在运行
fn acquire_single_instance() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::core::w;
    use windows::core::Error;
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        match CreateMutexW(
            None,
            true,
            w!("Local\\WSLImageClipboardHelper.SingleInstance"),
        ) {
            Ok(handle) if Error::from_win32().code() == ERROR_ALREADY_EXISTS.to_hresult() => {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                None
            }
            Ok(handle) => Some(handle),
            // 已有实例以管理员身份运行时，普通权限打不开它的 mutex
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => None,
            // 其他失败不应挡住启动
            Err(e) => {
                warn!("创建单实例 mutex 失败，跳过单实例检查: {}", e);
                Some(windows::Win32::Foundation::HANDLE::default())
            }
        }
    }
}

fn notify_already_running() {
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    unsafe {
        MessageBoxW(
            None,
            w!("WSL Clipboard Helper 已经在运行了，请在任务栏右下角的托盘区找它的图标。"),
            w!("WSL Clipboard Helper"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// 处理粘贴操作
async fn handle_paste(
    clipboard_manager: &ClipboardManager,
    uploader: &RemoteUploader,
    mode: &RuntimeMode,
    path_style: PathStyle,
    ime_protection: ImeProtection,
    session: JoinHandle<Result<Option<SshSession>>>,
) -> Result<()> {
    let paste_window = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    // 0. 立即注入菜单屏蔽键：赶在物理 Alt 抬起之前，
    //    避免目标窗口把"Alt 按下→抬起"当成单击 Alt 激活菜单栏
    paste::mask_alt_tap();

    // 1. 真实图片或本程序为上一张图片写入的路径，都进入图片处理链
    let image = if clipboard_manager.has_image() {
        Some(
            clipboard_manager
                .read_image_for_paste()
                .ok_or_else(|| anyhow::anyhow!("读取剪贴板图片失败"))?,
        )
    } else {
        clipboard_manager.read_own_image_text()?
    };
    let Some(image) = image else {
        if clipboard_manager.has_file_list() {
            let file_source_seq =
                unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
            let remote = resolve_paste_session(session).await?;
            let paths = match remote.as_ref() {
                // 发现了 ssh 会话：Explorer 里复制的文件同样上传过去，粘贴远程路径
                Some(target) => match clipboard_manager.read_file_list_raw() {
                    Some(win_paths) => match upload_file_list(uploader, target, &win_paths).await {
                        Ok(remote_paths) => Some(remote_paths),
                        Err(e) => {
                            report_remote_failure(target, &e);
                            return Ok(());
                        }
                    },
                    None => None,
                },
                None => clipboard_manager.read_file_list_for_paste(),
            };

            if let Some(wsl_paths) = paths {
                if !still_in_paste_window(paste_window) {
                    return Ok(());
                }
                ensure_target_tab(remote.as_ref())?;
                let paste_text = path_style.format_paths(&wsl_paths);

                let _ime_guard = match mode {
                    RuntimeMode::Safe => Some(paste::ImeGuard::new(ime_protection)),
                    RuntimeMode::Fast => None,
                };

                info!("粘贴文件路径: {}", paste_text);
                if paste::write_text(&paste_text, Some(file_source_seq))?.is_some() {
                    paste::send_paste()?;
                }
                return Ok(());
            }
        }

        info!("剪贴板无图片，执行普通粘贴");
        paste::send_paste()?;
        return Ok(());
    };

    let clipboard::PasteImage {
        source_seq,
        win_path,
        wsl_path,
        png_data,
    } = image;

    // 3. 先落盘再粘贴：CLI 收到路径的瞬间会检查文件是否存在，
    //    决定渲染成 [Image #n] 还是留下原始路径文本（issue #4）
    info!(
        "保存图片: {} bytes → {}",
        png_data.len(),
        win_path.display()
    );
    image_saver::ensure_saved(&win_path, &png_data).await?;

    // 3b. 发现了 ssh 会话（issue #11）：先把图片 ssh 上传到远程，再粘贴远程路径；
    //     上传失败时不粘贴任何内容——本地 /mnt 路径在远程终端里没有意义
    let target = resolve_paste_session(session).await?;
    let paste_path = match target.as_ref() {
        Some(target) => match uploader.upload(target, &win_path, true).await {
            Ok(remote_path) => remote_path,
            Err(e) => {
                report_remote_failure(&target, &e);
                return Ok(());
            }
        },
        None => wsl_path,
    };

    // 4. 输入法保护（仅安全模式，且由 ime_protection 策略决定具体手段）
    if !still_in_paste_window(paste_window) {
        return Ok(());
    }
    ensure_target_tab(target.as_ref())?;
    let _ime_guard = match mode {
        RuntimeMode::Safe => Some(paste::ImeGuard::new(ime_protection)),
        RuntimeMode::Fast => None,
    };

    // 5. 粘贴路径（本地 WSL 路径或远程路径）
    let paste_text = path_style.format_paths(std::slice::from_ref(&paste_path));
    info!("粘贴路径: {}", paste_text);
    let Some(written_seq) = paste::write_text(&paste_text, Some(source_seq))? else {
        info!("图片处理期间剪贴板已变化，取消本次粘贴");
        return Ok(());
    };
    clipboard_manager.record_image_text(source_seq, written_seq, &paste_text);
    paste::send_paste()?;

    // 6. ImeGuard 在此处 drop，触发 120ms 后恢复输入法

    Ok(())
}

async fn resolve_paste_session(
    task: JoinHandle<Result<Option<SshSession>>>,
) -> Result<Option<SshSession>> {
    match task.await {
        Ok(Ok(session)) => Ok(session),
        result => {
            let message = match result {
                Ok(Err(e)) => format!("{e:#}"),
                Err(e) => format!("无法识别当前 SSH 会话：{e}"),
                _ => unreachable!(),
            };
            tray::notify_warning("远程粘贴未完成", &message);
            anyhow::bail!("{message}")
        }
    }
}

fn ensure_target_tab(target: Option<&SshSession>) -> Result<()> {
    if let Some(target) = target {
        if target.client == ssh_clients::Client::MobaXterm
            && !foreground::embedded_session_active(target.pid)
        {
            let message = "MobaXterm 标签页已变化，请回到原 SSH 标签页再次按热键。";
            tray::notify_warning("已暂停粘贴", message);
            anyhow::bail!("{message}");
        }
    }
    Ok(())
}

/// 把 Explorer 复制的文件逐个上传到远程（跳过目录），返回远程路径列表
async fn upload_file_list(
    uploader: &RemoteUploader,
    target: &SshSession,
    win_paths: &[String],
) -> Result<Vec<String>> {
    let mut remote_paths = Vec::with_capacity(win_paths.len());
    for p in win_paths {
        let path = std::path::Path::new(p);
        if path.is_dir() {
            warn!("远程粘贴跳过目录: {}", p);
            continue;
        }
        remote_paths.push(uploader.upload(target, path, false).await?);
    }
    if remote_paths.is_empty() {
        anyhow::bail!("没有可上传的文件（不支持目录）");
    }
    Ok(remote_paths)
}

/// 远程上传失败：程序没有窗口，光写日志用户看不到，用托盘气泡告知原因。
fn report_remote_failure(session: &SshSession, err: &anyhow::Error) {
    error!("远程上传失败 ({}): {:#}", session.label(), err);
    let text = format!(
        "[{}] {:#}\n若取消认证或连接断开，可回到原终端再次按热键重试。",
        session.destination, err
    );
    tray::notify_warning("远程粘贴失败", &text);
}

fn still_in_paste_window(original: windows::Win32::Foundation::HWND) -> bool {
    if unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } == original {
        return true;
    }
    tray::notify_warning("已暂停粘贴", "当前窗口已变化，请回到原终端再次按热键粘贴。");
    false
}

#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

mod clipboard;
mod cleanup;
mod config;
mod hotkey;
mod image_saver;
mod paste;
mod tray;

use clipboard::ClipboardManager;
use config::{PathStyle, RuntimeMode};
use paste::HKL;
use tray::TrayCommand;

/// 应用运行时状态（可被托盘命令修改）
struct AppState {
    runtime_mode: RuntimeMode,
    path_style: PathStyle,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    info!(
        "WSL Clipboard Helper v{} (Rust) 启动中...",
        env!("CARGO_PKG_VERSION")
    );

    // 加载配置
    let app_config = config::AppConfig::load().unwrap_or_default();
    info!(
        "加载配置: 热键={}, 模式={:?}",
        app_config.hotkey, app_config.runtime_mode
    );

    // 确定临时目录
    let temp_dir = cleanup::temp_dir_from_current_exe()?;

    if !temp_dir.exists() {
        std::fs::create_dir_all(&temp_dir).context("创建临时目录失败")?;
    }

    info!("临时目录: {}", temp_dir.display());

    // 预加载英文输入法
    let english_hkl = paste::preload_english_layout();
    info!("英文输入法 HKL: {:#x}", english_hkl);

    // 创建剪贴板管理器
    let clipboard_manager = ClipboardManager::new(temp_dir.clone());

    // 运行时状态
    let state = Arc::new(Mutex::new(AppState {
        runtime_mode: app_config.runtime_mode.clone(),
        path_style: app_config.path_style,
    }));

    // 启动托盘（含热键管理器）
    let std_tray_rx = tray::TrayController::start(app_config, temp_dir.clone())?;

    // 将 std mpsc 桥接到 tokio mpsc，以便在 select! 中使用
    let (tray_tx_bridge, mut tray_rx) = mpsc::channel::<TrayCommand>(32);
    tokio::task::spawn_blocking(move || {
        loop {
            match std_tray_rx.recv() {
                Ok(cmd) => {
                    if tray_tx_bridge.blocking_send(cmd).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
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

    // 主事件循环
    loop {
        tokio::select! {
            // 热键触发
            Some(_hotkey_id) = hotkey_rx.recv() => {
                let (mode, path_style) = {
                    let s = state.lock().await;
                    (s.runtime_mode.clone(), s.path_style)
                };
                match handle_paste(&clipboard_manager, &mode, path_style, english_hkl).await {
                    Ok(_) => {}
                    Err(e) => {
                        error!("粘贴处理失败: {}", e);
                    }
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

    // 退出前清理 temp 目录下的所有 PNG 文件
    if let Err(e) = cleanup::cleanup_temp_png(&temp_dir) {
        warn!("退出清理临时文件失败: {}", e);
    }

    info!("WSL Clipboard Helper 已退出");
    std::process::exit(0);
}

/// 处理粘贴操作
async fn handle_paste(
    clipboard_manager: &ClipboardManager,
    mode: &RuntimeMode,
    path_style: PathStyle,
    english_hkl: HKL,
) -> Result<()> {
    // 0. 立即注入菜单屏蔽键：赶在物理 Alt 抬起之前，
    //    避免目标窗口把"Alt 按下→抬起"当成单击 Alt 激活菜单栏
    paste::mask_alt_tap();

    // 1. 检查剪贴板是否有图片
    if !clipboard_manager.has_image() {
        if clipboard_manager.has_file_list() {
            if let Some(wsl_paths) = clipboard_manager.read_file_list_for_paste() {
                let paste_text = path_style.format_paths(&wsl_paths);

                let _ime_guard = match mode {
                    RuntimeMode::Safe => Some(paste::ImeGuard::new(english_hkl)?),
                    RuntimeMode::Fast => None,
                };

                info!("粘贴文件路径: {}", paste_text);
                paste::paste_text(&paste_text)?;
                return Ok(());
            }
        }

        info!("剪贴板无图片，执行普通粘贴");
        paste::send_ctrl_v()?;
        return Ok(());
    }

    info!("检测到剪贴板图片");

    // 2. 读取图片（含缓存）
    let (win_path, wsl_path, png_data) = clipboard_manager
        .read_image_for_paste()
        .ok_or_else(|| anyhow::anyhow!("读取剪贴板图片失败"))?;

    // 3. 先落盘再粘贴：CLI 收到路径的瞬间会检查文件是否存在，
    //    决定渲染成 [Image #n] 还是留下原始路径文本（issue #4）
    info!("保存图片: {} bytes → {}", png_data.len(), win_path.display());
    image_saver::ensure_saved(&win_path, &png_data).await?;

    // 4. 输入法保护（仅安全模式）
    let _ime_guard = match mode {
        RuntimeMode::Safe => Some(paste::ImeGuard::new(english_hkl)?),
        RuntimeMode::Fast => None,
    };

    // 5. 粘贴 WSL 路径
    let paste_text = path_style.format_paths(std::slice::from_ref(&wsl_path));
    info!("粘贴路径: {}", paste_text);
    paste::paste_text(&paste_text)?;

    // 6. ImeGuard 在此处 drop，触发 120ms 后恢复输入法

    Ok(())
}

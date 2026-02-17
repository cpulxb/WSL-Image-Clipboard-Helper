#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

mod clipboard;
mod config;
mod hotkey;
mod image_saver;
mod paste;
mod tray;

use clipboard::ClipboardManager;
use config::RuntimeMode;
use paste::HKL;
use tray::TrayCommand;

/// 应用运行时状态（可被托盘命令修改）
struct AppState {
    runtime_mode: RuntimeMode,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    info!("WSL Clipboard Helper v2.0.0 (Rust) 启动中...");

    // 加载配置
    let app_config = config::AppConfig::load().unwrap_or_default();
    info!(
        "加载配置: 热键={}, 模式={:?}",
        app_config.hotkey, app_config.runtime_mode
    );

    // 确定临时目录
    let temp_dir = std::env::current_exe()
        .context("获取可执行文件路径失败")?
        .parent()
        .map(|p| p.join("temp"))
        .unwrap_or_else(|| PathBuf::from("./temp"));

    if !temp_dir.exists() {
        std::fs::create_dir_all(&temp_dir).context("创建临时目录失败")?;
    }

    info!("临时目录: {}", temp_dir.display());

    // 预加载英文输入法
    let english_hkl = paste::preload_english_layout();
    info!("英文输入法 HKL: {:#x}", english_hkl);

    // 创建剪贴板管理器
    let clipboard_manager = ClipboardManager::new(temp_dir.clone());

    // 启动图片保存异步任务（不再需要 temp_dir 参数）
    let save_tx = image_saver::start_saver();

    // 运行时状态
    let state = Arc::new(Mutex::new(AppState {
        runtime_mode: app_config.runtime_mode.clone(),
    }));

    // 启动托盘（含热键管理器）
    let std_tray_rx = tray::TrayController::start(app_config)?;

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
            if let Err(e) = cleanup_old_files(&temp_dir_for_cleanup) {
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
                let mode = {
                    let s = state.lock().await;
                    s.runtime_mode.clone()
                };
                match handle_paste(&clipboard_manager, &save_tx, &mode, english_hkl).await {
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
    if let Err(e) = cleanup_temp_png(&temp_dir) {
        warn!("退出清理临时文件失败: {}", e);
    }

    info!("WSL Clipboard Helper 已退出");
    std::process::exit(0);
}

/// 处理粘贴操作
async fn handle_paste(
    clipboard_manager: &ClipboardManager,
    save_tx: &mpsc::Sender<(PathBuf, Vec<u8>)>,
    mode: &RuntimeMode,
    english_hkl: HKL,
) -> Result<()> {
    // 1. 检查剪贴板是否有图片
    if !clipboard_manager.has_image() {
        info!("剪贴板无图片，执行普通粘贴");
        paste::release_all_modifiers();
        paste::send_ctrl_v()?;
        return Ok(());
    }

    info!("检测到剪贴板图片");

    // 2. 读取图片（含缓存）
    let (win_path, wsl_path, png_data) = clipboard_manager
        .read_image_for_paste()
        .ok_or_else(|| anyhow::anyhow!("读取剪贴板图片失败"))?;

    // 3. 输入法保护（仅安全模式）
    let _ime_guard = match mode {
        RuntimeMode::Safe => Some(paste::ImeGuard::new(english_hkl)?),
        RuntimeMode::Fast => None,
    };

    // 4. 粘贴 WSL 路径
    info!("粘贴路径: {}", wsl_path);
    paste::paste_text(&wsl_path)?;

    // 5. 异步保存图片
    info!("保存图片: {} bytes → {}", png_data.len(), win_path.display());
    let _ = save_tx.send((win_path, png_data)).await;

    // 6. ImeGuard 在此处 drop，触发 120ms 后恢复输入法

    Ok(())
}

/// 退出时清理 temp 目录下的所有 PNG 文件
fn cleanup_temp_png(temp_dir: &std::path::Path) -> Result<()> {
    use std::fs;

    if !temp_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(temp_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            == Some("png".to_string())
        {
            if let Err(e) = fs::remove_file(&path) {
                warn!("删除临时文件失败 {}: {}", path.display(), e);
            } else {
                info!("退出清理: {}", path.display());
            }
        }
    }

    Ok(())
}

/// 清理超过 2 小时的临时文件
fn cleanup_old_files<T: AsRef<std::path::Path>>(temp_dir: T) -> Result<()> {
    use std::fs;
    let temp_dir = temp_dir.as_ref();
    let now = chrono::Local::now();

    if !temp_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(temp_dir)? {
        let entry = entry?;
        let path = entry.path();

        // 只清理 PNG 文件
        if path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            != Some("png".to_string())
        {
            continue;
        }

        if let Ok(metadata) = entry.metadata() {
            if let Ok(modified) = metadata.modified() {
                let modified_time = chrono::DateTime::<chrono::Local>::from(modified);
                let hours_old = now.signed_duration_since(modified_time).num_hours();

                if hours_old > 2 {
                    let _ = fs::remove_file(&path);
                    info!("删除过期文件: {}", path.display());
                }
            }
        }
    }

    Ok(())
}

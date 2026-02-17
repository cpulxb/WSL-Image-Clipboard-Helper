use crate::config::{AppConfig, RuntimeMode};
use crate::hotkey::{HotkeyManager, HotkeyType};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use tracing::{error, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 托盘图标回调消息
const WM_TRAYICON: u32 = WM_APP + 100;

/// 菜单命令 ID
const CMD_HOTKEY_ALTV: u32 = 1001;
const CMD_HOTKEY_CTRLALTV: u32 = 1002;
const CMD_HOTKEY_ALTENTER: u32 = 1003;
const CMD_MODE_SAFE: u32 = 2001;
const CMD_MODE_FAST: u32 = 2002;
const CMD_OPEN_FOLDER: u32 = 3001;
const CMD_EXIT: u32 = 4001;

/// 托盘发往主循环的命令
#[derive(Debug, Clone)]
pub enum TrayCommand {
    SwitchHotkey(HotkeyType),
    SwitchMode(RuntimeMode),
    OpenFolder,
    Exit,
}

/// 线程局部存储：用于在 wnd_proc 中访问状态
struct TrayState {
    nid: NOTIFYICONDATAW,
    config: AppConfig,
    hotkey_manager: HotkeyManager,
    cmd_tx: std_mpsc::Sender<TrayCommand>,
}

// 全局状态指针（仅托盘线程访问）
static mut TRAY_STATE: *mut TrayState = std::ptr::null_mut();

/// 托盘控制器
pub struct TrayController;

impl TrayController {
    /// 启动托盘线程，返回命令接收端和一个 join handle
    /// 热键管理器在托盘线程上创建（需要 Win32 消息循环）
    pub fn start(config: AppConfig) -> Result<std_mpsc::Receiver<TrayCommand>> {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<TrayCommand>();

        let config_clone = config.clone();
        let tx = cmd_tx.clone();

        std::thread::Builder::new()
            .name("tray-thread".to_string())
            .spawn(move || {
                if let Err(e) = run_tray_thread(config_clone, tx) {
                    error!("托盘线程异常退出: {}", e);
                }
            })
            .context("启动托盘线程失败")?;

        Ok(cmd_rx)
    }
}

/// 托盘线程主函数
fn run_tray_thread(config: AppConfig, cmd_tx: std_mpsc::Sender<TrayCommand>) -> Result<()> {
    unsafe {
        let h_instance = GetModuleHandleW(None)?;

        // 注册窗口类
        let class_name_buf: Vec<u16> = "WSLClipboardTray\0".encode_utf16().collect();
        let class_name = PCWSTR::from_raw(class_name_buf.as_ptr());

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: Default::default(),
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: h_instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };

        RegisterClassExW(&wc);

        let window_name_buf: Vec<u16> = "WSL Clipboard Tray\0".encode_utf16().collect();

        // 创建隐藏窗口
        let hwnd = CreateWindowExW(
            Default::default(),
            class_name,
            PCWSTR::from_raw(window_name_buf.as_ptr()),
            Default::default(),
            0, 0, 0, 0,
            None,
            None,
            h_instance,
            None,
        );

        if hwnd.0 == 0 {
            anyhow::bail!("创建隐藏窗口失败");
        }

        // 创建托盘图标
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = LoadIconW(None, IDI_APPLICATION)?;

        // 设置 tooltip
        set_tooltip(&mut nid, &config);

        Shell_NotifyIconW(NIM_ADD, &nid);

        // 在托盘线程上创建热键管理器
        let mut hotkey_manager = HotkeyManager::new()?;

        // 注册初始热键
        let initial_hotkey = HotkeyType::from_config(&config.hotkey)
            .unwrap_or(HotkeyType::AltV);
        if let Err(e) = hotkey_manager.register(initial_hotkey) {
            warn!("注册初始热键失败: {}", e);
        }

        // 创建状态
        let mut state = Box::new(TrayState {
            nid,
            config,
            hotkey_manager,
            cmd_tx,
        });

        TRAY_STATE = &mut *state as *mut TrayState;

        // 消息循环
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
            if ret.0 <= 0 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 清理
        if let Err(e) = state.hotkey_manager.unregister() {
            warn!("退出时注销热键失败: {}", e);
        }
        Shell_NotifyIconW(NIM_DELETE, &state.nid);
        let _ = DestroyWindow(hwnd);
        let _ = UnregisterClassW(class_name, h_instance);
        TRAY_STATE = std::ptr::null_mut();

        info!("托盘线程已退出");
    }

    Ok(())
}

/// 设置 tooltip 文本
fn set_tooltip(nid: &mut NOTIFYICONDATAW, config: &AppConfig) {
    let hotkey_display = HotkeyType::from_config(&config.hotkey)
        .map(|h| h.display_name())
        .unwrap_or("Alt+V");

    let mode_display = match &config.runtime_mode {
        RuntimeMode::Safe => "稳定",
        RuntimeMode::Fast => "极速",
    };

    let tip = format!("WSL Clipboard ({} | {})", hotkey_display, mode_display);
    let tip_utf16: Vec<u16> = tip.encode_utf16().collect();
    let len = tip_utf16.len().min(nid.szTip.len() - 1);
    nid.szTip[..len].copy_from_slice(&tip_utf16[..len]);
    nid.szTip[len] = 0;
}

/// 窗口过程
unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TRAYICON {
        let mouse_msg = (lparam.0 & 0xFFFF) as u32;
        if mouse_msg == WM_RBUTTONUP || mouse_msg == WM_CONTEXTMENU {
            show_context_menu(hwnd);
        }
        return LRESULT(0);
    }

    if msg == WM_COMMAND {
        let cmd_id = (wparam.0 & 0xFFFF) as u32;
        handle_menu_command(cmd_id);
        return LRESULT(0);
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 显示右键菜单
unsafe fn show_context_menu(hwnd: HWND) {
    if TRAY_STATE.is_null() {
        return;
    }
    let state = &*TRAY_STATE;

    let h_menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    // ---- 热键子菜单 ----
    let h_hotkey_menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => { let _ = DestroyMenu(h_menu); return; }
    };

    let current_hotkey = state.hotkey_manager.current_type();
    for ht in HotkeyType::all() {
        let label = format!("{}\0", ht.display_name());
        let label_w: Vec<u16> = label.encode_utf16().collect();
        let cmd_id = match ht {
            HotkeyType::AltV => CMD_HOTKEY_ALTV,
            HotkeyType::CtrlAltV => CMD_HOTKEY_CTRLALTV,
            HotkeyType::AltEnter => CMD_HOTKEY_ALTENTER,
        };
        let mut flags = MF_STRING;
        if *ht == current_hotkey {
            flags |= MF_CHECKED;
        }
        let _ = AppendMenuW(h_hotkey_menu, flags, cmd_id as usize, PCWSTR::from_raw(label_w.as_ptr()));
    }

    let hotkey_label: Vec<u16> = "切换快捷键\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_POPUP, h_hotkey_menu.0 as usize, PCWSTR::from_raw(hotkey_label.as_ptr()));

    // ---- 模式子菜单 ----
    let h_mode_menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => { let _ = DestroyMenu(h_menu); return; }
    };

    let is_safe = matches!(state.config.runtime_mode, RuntimeMode::Safe);

    let safe_label: Vec<u16> = "稳定模式（输入法保护）\0".encode_utf16().collect();
    let safe_flags = MF_STRING | if is_safe { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(h_mode_menu, safe_flags, CMD_MODE_SAFE as usize, PCWSTR::from_raw(safe_label.as_ptr()));

    let fast_label: Vec<u16> = "极速模式（更快）\0".encode_utf16().collect();
    let fast_flags = MF_STRING | if !is_safe { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(h_mode_menu, fast_flags, CMD_MODE_FAST as usize, PCWSTR::from_raw(fast_label.as_ptr()));

    let mode_label: Vec<u16> = "运行模式\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_POPUP, h_mode_menu.0 as usize, PCWSTR::from_raw(mode_label.as_ptr()));

    // ---- 分隔线 ----
    let _ = AppendMenuW(h_menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ---- 打开缓存 ----
    let folder_label: Vec<u16> = "打开图片缓存\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_STRING, CMD_OPEN_FOLDER as usize, PCWSTR::from_raw(folder_label.as_ptr()));

    // ---- 分隔线 ----
    let _ = AppendMenuW(h_menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ---- 退出 ----
    let exit_label: Vec<u16> = "退出\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_STRING, CMD_EXIT as usize, PCWSTR::from_raw(exit_label.as_ptr()));

    // 显示菜单
    let mut pt = windows::Win32::Foundation::POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);

    TrackPopupMenu(h_menu, TPM_LEFTALIGN | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);

    let _ = DestroyMenu(h_menu);
}

/// 处理菜单命令
unsafe fn handle_menu_command(cmd_id: u32) {
    if TRAY_STATE.is_null() {
        return;
    }
    let state = &mut *TRAY_STATE;

    match cmd_id {
        CMD_HOTKEY_ALTV => switch_hotkey(state, HotkeyType::AltV),
        CMD_HOTKEY_CTRLALTV => switch_hotkey(state, HotkeyType::CtrlAltV),
        CMD_HOTKEY_ALTENTER => switch_hotkey(state, HotkeyType::AltEnter),
        CMD_MODE_SAFE => switch_mode(state, RuntimeMode::Safe),
        CMD_MODE_FAST => switch_mode(state, RuntimeMode::Fast),
        CMD_OPEN_FOLDER => {
            let _ = state.cmd_tx.send(TrayCommand::OpenFolder);
        }
        CMD_EXIT => {
            let _ = state.cmd_tx.send(TrayCommand::Exit);
            PostQuitMessage(0);
        }
        _ => {}
    }
}

/// 切换热键
unsafe fn switch_hotkey(state: &mut TrayState, hotkey_type: HotkeyType) {
    if state.hotkey_manager.current_type() == hotkey_type {
        return;
    }

    if let Err(e) = state.hotkey_manager.register(hotkey_type) {
        error!("切换热键失败: {}", e);
        return;
    }

    state.config.hotkey = hotkey_type.as_str().to_string();
    let _ = state.config.save();

    // 更新 tooltip
    set_tooltip(&mut state.nid, &state.config);
    state.nid.uFlags = NIF_TIP;
    Shell_NotifyIconW(NIM_MODIFY, &state.nid);

    let _ = state.cmd_tx.send(TrayCommand::SwitchHotkey(hotkey_type));
    info!("已切换热键: {}", hotkey_type.display_name());
}

/// 切换模式
unsafe fn switch_mode(state: &mut TrayState, mode: RuntimeMode) {
    let mode_str = match &mode {
        RuntimeMode::Safe => "safe",
        RuntimeMode::Fast => "fast",
    };
    let current_str = match &state.config.runtime_mode {
        RuntimeMode::Safe => "safe",
        RuntimeMode::Fast => "fast",
    };

    if mode_str == current_str {
        return;
    }

    state.config.runtime_mode = mode.clone();
    let _ = state.config.save();

    // 更新 tooltip
    set_tooltip(&mut state.nid, &state.config);
    state.nid.uFlags = NIF_TIP;
    Shell_NotifyIconW(NIM_MODIFY, &state.nid);

    let _ = state.cmd_tx.send(TrayCommand::SwitchMode(mode));
    info!("已切换模式: {}", mode_str);
}

/// 打开临时文件夹
pub fn open_temp_folder() -> Result<()> {
    let temp_dir = std::env::current_exe()
        .context("获取可执行文件路径失败")?
        .parent()
        .map(|p| p.join("temp"))
        .unwrap_or_else(|| PathBuf::from(".\\temp"));

    if temp_dir.exists() {
        std::process::Command::new("explorer.exe")
            .arg(temp_dir.to_string_lossy().to_string())
            .spawn()
            .context("打开文件夹失败")?;
    }

    Ok(())
}

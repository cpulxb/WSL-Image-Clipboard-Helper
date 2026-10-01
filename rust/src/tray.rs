use crate::cleanup;
use crate::config::{AppConfig, ImeProtection, PathStyle, RuntimeMode};
use crate::hotkey::{HotkeyManager, HotkeyType};
use crate::remote::{self, RemoteMode, SshSession};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc as std_mpsc;
use tracing::{error, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 托盘图标回调消息
const WM_TRAYICON: u32 = WM_APP + 100;
/// 其他线程请求弹托盘气泡提示：lparam 为 Box<(标题, 正文)> 的裸指针
const WM_TRAY_NOTIFY: u32 = WM_APP + 101;
const APP_ICON_ID: u16 = 1;

/// 菜单命令 ID
const CMD_HOTKEY_ALTV: u32 = 1001;
const CMD_HOTKEY_CTRLALTV: u32 = 1002;
const CMD_HOTKEY_ALTENTER: u32 = 1003;
const CMD_MODE_SAFE: u32 = 2001;
const CMD_MODE_FAST: u32 = 2002;
const CMD_OPEN_FOLDER: u32 = 3001;
const CMD_EXIT: u32 = 4001;
const CMD_STYLE_PLAIN: u32 = 5001;
const CMD_STYLE_AT: u32 = 5002;
const CMD_STYLE_QUOTED: u32 = 5003;
const CMD_IME_OFF: u32 = 6001;
const CMD_IME_IMM: u32 = 6002;
const CMD_IME_LAYOUT: u32 = 6003;
const CMD_IME_LAYOUT_FORCE: u32 = 6004;
const CMD_REMOTE_OFF: u32 = 7001;
const CMD_REMOTE_AUTO: u32 = 7002;
/// 打开菜单时发现的 ssh 会话按顺序占用 CMD_REMOTE_BASE + 下标
const CMD_REMOTE_BASE: u32 = 7100;
const CMD_REMOTE_MAX: u32 = 7999;

/// 托盘发往主循环的命令
#[derive(Debug, Clone)]
pub enum TrayCommand {
    SwitchHotkey(HotkeyType),
    SwitchMode(RuntimeMode),
    SwitchPathStyle(PathStyle),
    SwitchImeProtection(ImeProtection),
    SwitchRemoteMode(RemoteMode),
    OpenFolder,
    Exit,
}

/// 线程局部存储：用于在 wnd_proc 中访问状态
struct TrayState {
    nid: NOTIFYICONDATAW,
    config: AppConfig,
    hotkey_manager: HotkeyManager,
    cmd_tx: std_mpsc::Sender<TrayCommand>,
    temp_dir: PathBuf,
    session_end_cleanup_done: bool,
    /// 远程粘贴模式（issue #11）；Off/Auto 持久化为 config.remote_paste，Pinned 只在本次运行有效
    remote_mode: RemoteMode,
    /// 上次打开菜单时发现的 ssh 会话，菜单项下标与之对应
    discovered: Vec<SshSession>,
}

// 全局状态指针（仅托盘线程访问）
static mut TRAY_STATE: *mut TrayState = std::ptr::null_mut();
// 托盘窗口句柄：供其他线程 PostMessage 请求气泡提示（0 = 托盘未就绪）
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// 托盘控制器
pub struct TrayController;

impl TrayController {
    /// 启动托盘线程，返回命令接收端和一个 join handle
    /// 热键管理器在托盘线程上创建（需要 Win32 消息循环）
    pub fn start(
        config: AppConfig,
        temp_dir: PathBuf,
    ) -> Result<std_mpsc::Receiver<TrayCommand>> {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<TrayCommand>();

        let config_clone = config.clone();
        let temp_dir_clone = temp_dir.clone();
        let tx = cmd_tx.clone();

        std::thread::Builder::new()
            .name("tray-thread".to_string())
            .spawn(move || {
                if let Err(e) = run_tray_thread(config_clone, temp_dir_clone, tx) {
                    error!("托盘线程异常退出: {}", e);
                }
            })
            .context("启动托盘线程失败")?;

        Ok(cmd_rx)
    }
}

unsafe fn load_app_icon(h_instance: HINSTANCE) -> windows::core::Result<HICON> {
    LoadIconW(h_instance, PCWSTR(APP_ICON_ID as usize as *const u16))
        .or_else(|_| LoadIconW(HINSTANCE::default(), IDI_APPLICATION))
}

/// 托盘线程主函数
fn run_tray_thread(
    config: AppConfig,
    temp_dir: PathBuf,
    cmd_tx: std_mpsc::Sender<TrayCommand>,
) -> Result<()> {
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
        TRAY_HWND.store(hwnd.0, Ordering::SeqCst);

        // 创建托盘图标
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = load_app_icon(h_instance.into())?;

        // 设置 tooltip
        let remote_paste = config.remote_paste;
        set_tooltip(&mut nid, &config, &RemoteMode::from_config(remote_paste));

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
            temp_dir,
            session_end_cleanup_done: false,
            remote_mode: RemoteMode::from_config(remote_paste),
            discovered: Vec::new(),
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
        TRAY_HWND.store(0, Ordering::SeqCst);
        Shell_NotifyIconW(NIM_DELETE, &state.nid);
        let _ = DestroyWindow(hwnd);
        let _ = UnregisterClassW(class_name, h_instance);
        TRAY_STATE = std::ptr::null_mut();

        info!("托盘线程已退出");
    }

    Ok(())
}

/// 设置 tooltip 文本
fn set_tooltip(nid: &mut NOTIFYICONDATAW, config: &AppConfig, remote_mode: &RemoteMode) {
    let hotkey_display = HotkeyType::from_config(&config.hotkey)
        .map(|h| h.display_name())
        .unwrap_or("Alt+V");

    let mode_display = match &config.runtime_mode {
        RuntimeMode::Safe => "兼容",
        RuntimeMode::Fast => "快速",
    };

    let tip = match remote_mode {
        RemoteMode::Off => format!("WSL Clipboard ({} | {})", hotkey_display, mode_display),
        RemoteMode::Auto => format!("WSL Clipboard ({} | {} | SSH:自动)", hotkey_display, mode_display),
        RemoteMode::Pinned(s) => format!(
            "WSL Clipboard ({} | {} | SSH:{})",
            hotkey_display, mode_display, s.destination
        ),
    };
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

    if msg == WM_TRAY_NOTIFY {
        let payload = Box::from_raw(lparam.0 as *mut (String, String));
        show_balloon(&payload.0, &payload.1);
        return LRESULT(0);
    }

    if msg == WM_QUERYENDSESSION {
        return LRESULT(1);
    }

    if msg == WM_ENDSESSION {
        if wparam.0 != 0 {
            handle_session_end();
        }
        return LRESULT(0);
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 从任意线程请求托盘弹出警告气泡（程序没有主窗口，这是用户唯一能看到的失败反馈）
pub fn notify_warning(title: &str, text: &str) {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }
    let payload = Box::new((title.to_string(), text.to_string()));
    let ptr = Box::into_raw(payload);
    unsafe {
        if PostMessageW(HWND(hwnd), WM_TRAY_NOTIFY, WPARAM(0), LPARAM(ptr as isize)).is_err() {
            // 投递失败则收回所有权，避免泄漏
            drop(Box::from_raw(ptr));
        }
    }
}

/// 托盘线程：弹出气泡提示（szInfo 最多 255 个 UTF-16 单元，超出截断）
unsafe fn show_balloon(title: &str, text: &str) {
    if TRAY_STATE.is_null() {
        return;
    }
    let state = &*TRAY_STATE;

    let mut nid = state.nid;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_WARNING;
    copy_utf16(&mut nid.szInfoTitle, title);
    copy_utf16(&mut nid.szInfo, text);
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

fn copy_utf16(dst: &mut [u16], src: &str) {
    let utf16: Vec<u16> = src.encode_utf16().collect();
    let len = utf16.len().min(dst.len() - 1);
    dst[..len].copy_from_slice(&utf16[..len]);
    dst[len] = 0;
}

unsafe fn handle_session_end() {
    if TRAY_STATE.is_null() {
        return;
    }

    let state = &mut *TRAY_STATE;
    if state.session_end_cleanup_done {
        return;
    }

    state.session_end_cleanup_done = true;
    info!("收到 Windows 会话结束消息，开始清理临时文件");

    if let Err(e) = cleanup::cleanup_temp_png(&state.temp_dir) {
        warn!("会话结束清理临时文件失败: {}", e);
    }

    let _ = state.cmd_tx.send(TrayCommand::Exit);
    PostQuitMessage(0);
}

/// 显示右键菜单
unsafe fn show_context_menu(hwnd: HWND) {
    if TRAY_STATE.is_null() {
        return;
    }
    let state = &mut *TRAY_STATE;

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

    let hotkey_label: Vec<u16> = "快捷键\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_POPUP, h_hotkey_menu.0 as usize, PCWSTR::from_raw(hotkey_label.as_ptr()));

    // ---- 模式子菜单 ----
    let h_mode_menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => { let _ = DestroyMenu(h_menu); return; }
    };

    let is_safe = matches!(state.config.runtime_mode, RuntimeMode::Safe);

    let safe_label: Vec<u16> = "兼容模式（输入法保护）\0".encode_utf16().collect();
    let safe_flags = MF_STRING | if is_safe { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(h_mode_menu, safe_flags, CMD_MODE_SAFE as usize, PCWSTR::from_raw(safe_label.as_ptr()));

    let fast_label: Vec<u16> = "快速模式（低延迟）\0".encode_utf16().collect();
    let fast_flags = MF_STRING | if !is_safe { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(h_mode_menu, fast_flags, CMD_MODE_FAST as usize, PCWSTR::from_raw(fast_label.as_ptr()));

    let mode_label: Vec<u16> = "运行模式\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_POPUP, h_mode_menu.0 as usize, PCWSTR::from_raw(mode_label.as_ptr()));

    // ---- 路径格式子菜单 ----
    let h_style_menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => { let _ = DestroyMenu(h_menu); return; }
    };

    let current_style = state.config.path_style;
    let style_items = [
        (PathStyle::Plain, CMD_STYLE_PLAIN),
        (PathStyle::At, CMD_STYLE_AT),
        (PathStyle::Quoted, CMD_STYLE_QUOTED),
    ];
    for (style, cmd_id) in style_items {
        let label = format!("{}\0", style.display_name());
        let label_w: Vec<u16> = label.encode_utf16().collect();
        let mut flags = MF_STRING;
        if style == current_style {
            flags |= MF_CHECKED;
        }
        let _ = AppendMenuW(h_style_menu, flags, cmd_id as usize, PCWSTR::from_raw(label_w.as_ptr()));
    }

    let style_label: Vec<u16> = "路径格式\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_POPUP, h_style_menu.0 as usize, PCWSTR::from_raw(style_label.as_ptr()));

    // ---- 输入法保护子菜单（issue #10）----
    // 兼容模式下具体用哪种手段保护输入法，可在这里彻底关掉
    let h_ime_menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => { let _ = DestroyMenu(h_menu); return; }
    };

    let current_ime = state.config.ime_protection;
    for policy in ImeProtection::all() {
        let label = format!("{}\0", policy.display_name());
        let label_w: Vec<u16> = label.encode_utf16().collect();
        let cmd_id = match policy {
            ImeProtection::Off => CMD_IME_OFF,
            ImeProtection::Imm => CMD_IME_IMM,
            ImeProtection::Layout => CMD_IME_LAYOUT,
            ImeProtection::LayoutForce => CMD_IME_LAYOUT_FORCE,
        };
        let mut flags = MF_STRING;
        if *policy == current_ime {
            flags |= MF_CHECKED;
        }
        // 输入法保护只在兼容模式下生效，快速模式下置灰以免误解
        if !is_safe {
            flags |= MF_GRAYED;
        }
        let _ = AppendMenuW(h_ime_menu, flags, cmd_id as usize, PCWSTR::from_raw(label_w.as_ptr()));
    }

    let ime_label: Vec<u16> = "输入法保护\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_POPUP, h_ime_menu.0 as usize, PCWSTR::from_raw(ime_label.as_ptr()));

    // ---- 远程粘贴子菜单（issue #11）----
    // 每次打开菜单都重新枚举当前的 ssh 会话，不需要用户配置目标
    let h_remote_menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => { let _ = DestroyMenu(h_menu); return; }
    };

    state.discovered = remote::discover_sessions();

    let auto_label: Vec<u16> = "自动（当前 tab 是 ssh 才上传）\0".encode_utf16().collect();
    let auto_flags = MF_STRING | if matches!(state.remote_mode, RemoteMode::Auto) { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(h_remote_menu, auto_flags, CMD_REMOTE_AUTO as usize, PCWSTR::from_raw(auto_label.as_ptr()));

    let off_label: Vec<u16> = "关闭（始终粘贴本地 /mnt 路径）\0".encode_utf16().collect();
    let off_flags = MF_STRING | if matches!(state.remote_mode, RemoteMode::Off) { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(h_remote_menu, off_flags, CMD_REMOTE_OFF as usize, PCWSTR::from_raw(off_label.as_ptr()));

    let _ = AppendMenuW(h_remote_menu, MF_SEPARATOR, 0, PCWSTR::null());
    if state.discovered.is_empty() {
        let hint: Vec<u16> = "当前没有打开的 ssh 会话\0".encode_utf16().collect();
        let _ = AppendMenuW(h_remote_menu, MF_STRING | MF_GRAYED, 0, PCWSTR::from_raw(hint.as_ptr()));
    }
    for (idx, session) in state.discovered.iter().enumerate() {
        let cmd_id = CMD_REMOTE_BASE + idx as u32;
        if cmd_id > CMD_REMOTE_MAX {
            break;
        }
        let label = format!("{}\0", session.label());
        let label_w: Vec<u16> = label.encode_utf16().collect();
        let mut flags = MF_STRING;
        if matches!(&state.remote_mode, RemoteMode::Pinned(p) if p.same_target(session)) {
            flags |= MF_CHECKED;
        }
        let _ = AppendMenuW(h_remote_menu, flags, cmd_id as usize, PCWSTR::from_raw(label_w.as_ptr()));
    }

    let remote_label: Vec<u16> = "远程粘贴（SSH）\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_POPUP, h_remote_menu.0 as usize, PCWSTR::from_raw(remote_label.as_ptr()));

    // ---- 分隔线 ----
    let _ = AppendMenuW(h_menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ---- 打开缓存 ----
    let folder_label: Vec<u16> = "打开临时图片目录\0".encode_utf16().collect();
    let _ = AppendMenuW(h_menu, MF_STRING, CMD_OPEN_FOLDER as usize, PCWSTR::from_raw(folder_label.as_ptr()));

    // ---- 分隔线 ----
    let _ = AppendMenuW(h_menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ---- 退出 ----
    let exit_label: Vec<u16> = "退出并清理临时图片\0".encode_utf16().collect();
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
        CMD_STYLE_PLAIN => switch_path_style(state, PathStyle::Plain),
        CMD_STYLE_AT => switch_path_style(state, PathStyle::At),
        CMD_STYLE_QUOTED => switch_path_style(state, PathStyle::Quoted),
        CMD_IME_OFF => switch_ime_protection(state, ImeProtection::Off),
        CMD_IME_IMM => switch_ime_protection(state, ImeProtection::Imm),
        CMD_IME_LAYOUT => switch_ime_protection(state, ImeProtection::Layout),
        CMD_IME_LAYOUT_FORCE => switch_ime_protection(state, ImeProtection::LayoutForce),
        CMD_REMOTE_OFF => switch_remote_mode(state, RemoteMode::Off),
        CMD_REMOTE_AUTO => switch_remote_mode(state, RemoteMode::Auto),
        CMD_REMOTE_BASE..=CMD_REMOTE_MAX => {
            let idx = (cmd_id - CMD_REMOTE_BASE) as usize;
            if let Some(session) = state.discovered.get(idx) {
                let session = session.clone();
                switch_remote_mode(state, RemoteMode::Pinned(Box::new(session)));
            }
        }
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
    set_tooltip(&mut state.nid, &state.config, &state.remote_mode);
    state.nid.uFlags = NIF_TIP;
    Shell_NotifyIconW(NIM_MODIFY, &state.nid);

    let _ = state.cmd_tx.send(TrayCommand::SwitchHotkey(hotkey_type));
    info!("已切换热键: {}", hotkey_type.display_name());
}

/// 切换路径格式
unsafe fn switch_path_style(state: &mut TrayState, style: PathStyle) {
    if state.config.path_style == style {
        return;
    }

    state.config.path_style = style;
    let _ = state.config.save();

    let _ = state.cmd_tx.send(TrayCommand::SwitchPathStyle(style));
    info!("已切换路径格式: {}", style.display_name());
}

/// 切换输入法保护策略
unsafe fn switch_ime_protection(state: &mut TrayState, policy: ImeProtection) {
    if state.config.ime_protection == policy {
        return;
    }

    state.config.ime_protection = policy;
    let _ = state.config.save();

    let _ = state.cmd_tx.send(TrayCommand::SwitchImeProtection(policy));
    info!("已切换输入法保护: {}", policy.display_name());
}

/// 切换远程粘贴模式；只有"开 / 关"写进配置，指定的会话不持久化
unsafe fn switch_remote_mode(state: &mut TrayState, mode: RemoteMode) {
    let enabled = !matches!(mode, RemoteMode::Off);
    if state.config.remote_paste != enabled {
        state.config.remote_paste = enabled;
        let _ = state.config.save();
    }
    state.remote_mode = mode.clone();

    // 更新 tooltip
    set_tooltip(&mut state.nid, &state.config, &state.remote_mode);
    state.nid.uFlags = NIF_TIP;
    Shell_NotifyIconW(NIM_MODIFY, &state.nid);

    info!("远程粘贴已切换为: {}", mode.display_name());
    let _ = state.cmd_tx.send(TrayCommand::SwitchRemoteMode(mode));
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
    set_tooltip(&mut state.nid, &state.config, &state.remote_mode);
    state.nid.uFlags = NIF_TIP;
    Shell_NotifyIconW(NIM_MODIFY, &state.nid);

    let _ = state.cmd_tx.send(TrayCommand::SwitchMode(mode));
    info!("已切换模式: {}", mode_str);
}

/// 打开临时文件夹
pub fn open_temp_folder() -> Result<()> {
    let temp_dir = cleanup::temp_dir_from_current_exe()?;

    if temp_dir.exists() {
        std::process::Command::new("explorer.exe")
            .arg(temp_dir.to_string_lossy().to_string())
            .spawn()
            .context("打开文件夹失败")?;
    }

    Ok(())
}

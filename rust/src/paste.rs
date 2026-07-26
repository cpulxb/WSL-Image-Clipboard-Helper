use anyhow::{bail, Result};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LMENU, VK_MENU, VK_RMENU, VK_SHIFT, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, PostMessageW, GUITHREADINFO,
    GUI_INMENUMODE,
};

use tracing::{info, warn};

/// HKL 类型别名（Win32 HKL 就是一个 isize）
pub type HKL = isize;

/// WM_INPUTLANGCHANGEREQUEST
const WM_INPUTLANGCHANGEREQUEST: u32 = 0x0050;

/// 预加载英文输入法布局，返回英文 HKL
/// 启动时调用一次即可
pub fn preload_english_layout() -> HKL {
    unsafe {
        let layout_str: Vec<u16> = "00000409\0".encode_utf16().collect();
        // KLF_ACTIVATE (0x1): 加载并激活英文布局，确保可用（与 AHK 一致）
        LoadKeyboardLayoutW(layout_str.as_ptr(), 0x01)
    }
}

#[link(name = "user32")]
extern "system" {
    fn LoadKeyboardLayoutW(pwszklid: *const u16, flags: u32) -> HKL;
    fn GetKeyboardLayout(idThread: u32) -> HKL;
}

/// 粘贴文本到剪贴板并执行粘贴操作
pub fn paste_text(text: &str) -> Result<()> {
    unsafe {
        // 尝试打开剪贴板，带重试机制
        if OpenClipboard(None).is_err() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if let Err(e) = OpenClipboard(None) {
                bail!("无法打开剪贴板: {:?}", e);
            }
        }

        // 清空剪贴板（关键修复：必须先清空再设置）
        if let Err(e) = EmptyClipboard() {
            CloseClipboard().ok();
            bail!("清空剪贴板失败: {:?}", e);
        }

        // 准备 UTF-16 编码的数据
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_len = utf16.len() * std::mem::size_of::<u16>();

        // 分配内存
        let h_mem = match GlobalAlloc(GMEM_MOVEABLE, byte_len) {
            Ok(mem) => mem,
            Err(e) => {
                CloseClipboard().ok();
                bail!("分配剪贴板内存失败: {:?}", e);
            }
        };

        let ptr = GlobalLock(h_mem);
        if ptr.is_null() {
            CloseClipboard().ok();
            bail!("锁定内存失败");
        }

        // 复制数据
        std::ptr::copy_nonoverlapping(utf16.as_ptr() as *const u8, ptr as *mut u8, byte_len);

        let _ = GlobalUnlock(h_mem);

        // 设置剪贴板数据
        if SetClipboardData(
            CF_UNICODETEXT.0 as u32,
            windows::Win32::Foundation::HANDLE(h_mem.0 as isize),
        )
        .is_err()
        {
            CloseClipboard().ok();
            bail!("设置剪贴板数据失败");
        }

        CloseClipboard().ok();
    }

    // 发送粘贴快捷键
    send_ctrl_v()?;
    Ok(())
}

/// 菜单屏蔽键：与 AHK 的 A_MenuMaskKey（vkFF）一致。
/// 热键触发时物理 Alt 往往仍按着，目标窗口只看到 Alt 按下（V 被热键吞掉），
/// 若紧接着注入 Alt 抬起，部分程序会当成“单击 Alt”激活菜单栏、抢走输入焦点，
/// 导致后续 Ctrl+V 粘不出内容。先注入一个无副作用的按键可打断该判定。
const VK_MENU_MASK: VIRTUAL_KEY = VIRTUAL_KEY(0xFF);

/// 检查某个虚拟键当前是否被物理按住
fn is_key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
}

/// 等待用户松开会污染 Ctrl+V 的物理修饰键（Alt/Shift），最多等待 timeout_ms。
/// 物理按住的 Alt 会随键盘自动重复重新置为按下状态，仅靠注入抬起事件不总是够，
/// 这是“偶发粘贴无内容”的常见来源之一。
fn wait_modifiers_released(timeout_ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while is_key_down(VK_MENU)
        || is_key_down(VK_LMENU)
        || is_key_down(VK_RMENU)
        || is_key_down(VK_SHIFT)
    {
        if std::time::Instant::now() >= deadline {
            warn!("等待修饰键释放超时，继续注入粘贴按键");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// 热键触发后应立即调用：抢在用户松开 Alt 之前插入屏蔽键。
/// RegisterHotKey 会吞掉 V 键事件，目标窗口只看到 Alt 按下/抬起；
/// 若屏蔽键能落在物理 Alt 抬起之前，窗口就不会把它当成"单击 Alt"而激活菜单栏。
/// 必须放在读剪贴板/写文件等耗时操作之前，越早越好。
pub fn mask_alt_tap() {
    let inputs = [
        make_key_input(VK_MENU_MASK, false),
        make_key_input(VK_MENU_MASK, true),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 若前台线程已进入菜单模式（快速松开 Alt 时屏蔽键会输掉竞速，菜单栏已被激活），
/// 先发送 Esc 退出菜单，否则注入的 Ctrl+V 会被菜单吃掉、粘贴无效
fn dismiss_menu_mode_if_active() {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0 == 0 {
            return;
        }
        let thread_id = GetWindowThreadProcessId(hwnd, None);
        if thread_id == 0 {
            return;
        }

        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(thread_id, &mut info).is_err() {
            return;
        }

        if (info.flags & GUI_INMENUMODE).0 != 0 {
            info!("前台窗口处于菜单模式，先发送 Esc 退出再粘贴");
            let esc = [
                make_key_input(VK_ESCAPE, false),
                make_key_input(VK_ESCAPE, true),
            ];
            SendInput(&esc, std::mem::size_of::<INPUT>() as i32);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
}

/// 释放所有修饰键（Alt/Ctrl/Shift），对应 AHK 的 NormalizeModifierStateBeforeSend
pub fn release_all_modifiers() {
    let inputs = [
        // 先打菜单屏蔽键，再抬 Alt，避免“单击 Alt”激活窗口菜单
        make_key_input(VK_MENU_MASK, false),
        make_key_input(VK_MENU_MASK, true),
        make_key_input(VK_MENU, true),     // Alt up
        make_key_input(VK_LMENU, true),    // Left Alt up
        make_key_input(VK_RMENU, true),    // Right Alt (AltGr) up
        make_key_input(VK_CONTROL, true),  // Ctrl up
        make_key_input(VK_SHIFT, true),    // Shift up
    ];

    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 发送 Ctrl+V 粘贴快捷键（使用 SendInput 替代 keybd_event）
pub fn send_ctrl_v() -> Result<()> {
    // 先给用户留出松开热键的时间（Alt+V/Alt+Enter 的 Alt 尚未抬起时，
    // 注入的 Ctrl+V 会被识别成 Ctrl+Alt+V 而失效）
    wait_modifiers_released(250);
    release_all_modifiers();
    // Alt 已被松开且菜单栏被激活时，Ctrl+V 会发进菜单而不是输入框
    dismiss_menu_mode_if_active();

    let inputs = [
        make_key_input(VK_CONTROL, false), // Ctrl down
        make_key_input(VK_V, false),       // V down
        make_key_input(VK_V, true),        // V up
        make_key_input(VK_CONTROL, true),  // Ctrl up
    ];

    unsafe {
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent != inputs.len() as u32 {
            bail!("SendInput 发送失败，期望 {} 实际 {}", inputs.len(), sent);
        }
    }

    Ok(())
}

/// 构造键盘 INPUT 结构
fn make_key_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0);
    if key_up {
        flags = KEYEVENTF_KEYUP;
    }

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 输入法保护器
/// 在粘贴路径前切换到英文输入法，完成后异步恢复
pub struct ImeGuard {
    /// 切换前的键盘布局句柄
    previous_hkl: HKL,
    /// 前台窗口句柄
    hwnd: HWND,
}

impl ImeGuard {
    /// 创建新的输入法保护器
    /// 获取当前输入法布局，切换到英文，保存旧布局用于恢复
    pub fn new(english_hkl: HKL) -> Result<Self> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0 == 0 {
                // 无前台窗口，跳过输入法切换
                return Ok(Self {
                    previous_hkl: 0,
                    hwnd: HWND::default(),
                });
            }

            let thread_id = GetWindowThreadProcessId(hwnd, None);
            if thread_id == 0 {
                return Ok(Self {
                    previous_hkl: 0,
                    hwnd: HWND::default(),
                });
            }

            let current_hkl = GetKeyboardLayout(thread_id);

            // 仅在当前布局不是英文时才切换
            if english_hkl != 0 && current_hkl != english_hkl {
                info!("ImeGuard: 切换输入法 {:#x} -> {:#x}", current_hkl, english_hkl);
                let _ = PostMessageW(
                    hwnd,
                    WM_INPUTLANGCHANGEREQUEST,
                    WPARAM(0),
                    LPARAM(english_hkl),
                );
                std::thread::sleep(std::time::Duration::from_millis(60));
            }

            Ok(Self {
                previous_hkl: current_hkl,
                hwnd,
            })
        }
    }
}

impl Drop for ImeGuard {
    /// 析构时异步恢复之前的键盘布局
    fn drop(&mut self) {
        if self.previous_hkl == 0 || self.hwnd.0 == 0 {
            return;
        }

        let hkl = self.previous_hkl;
        let hwnd = self.hwnd;

        // 在后台线程中延迟恢复，不阻塞主线程
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            unsafe {
                if let Err(e) = PostMessageW(
                    hwnd,
                    WM_INPUTLANGCHANGEREQUEST,
                    WPARAM(0),
                    LPARAM(hkl),
                ) {
                    warn!("恢复输入法失败: {:?}", e);
                } else {
                    info!("ImeGuard: 已恢复输入法 {:#x}", hkl);
                }
            }
        });
    }
}

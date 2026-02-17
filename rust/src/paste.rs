use anyhow::{bail, Result};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL,
    VK_LMENU, VK_MENU, VK_SHIFT, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, PostMessageW,
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

/// 释放所有修饰键（Alt/Ctrl/Shift），对应 AHK 的 NormalizeModifierStateBeforeSend
pub fn release_all_modifiers() {
    let inputs = [
        make_key_input(VK_MENU, true),    // Alt up
        make_key_input(VK_LMENU, true),   // Left Alt up
        make_key_input(VK_CONTROL, true),  // Ctrl up
        make_key_input(VK_SHIFT, true),    // Shift up
    ];

    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 发送 Ctrl+V 粘贴快捷键（使用 SendInput 替代 keybd_event）
pub fn send_ctrl_v() -> Result<()> {
    release_all_modifiers();

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

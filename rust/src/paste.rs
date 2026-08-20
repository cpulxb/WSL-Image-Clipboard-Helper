use anyhow::{bail, Result};
use std::sync::atomic::{AtomicIsize, Ordering};
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

use crate::config::ImeProtection;

/// HKL 类型别名（Win32 HKL 就是一个 isize）
pub type HKL = isize;

/// WM_INPUTLANGCHANGEREQUEST
const WM_INPUTLANGCHANGEREQUEST: u32 = 0x0050;

/// WM_IME_CONTROL 及其子命令，用于在**不改动键盘布局**的前提下
/// 关闭 / 恢复前台窗口的 IME 输入状态
const WM_IME_CONTROL: u32 = 0x0283;
const IMC_GETOPENSTATUS: usize = 0x0005;
const IMC_SETOPENSTATUS: usize = 0x0006;

/// LoadKeyboardLayoutW 的标志位。
///
/// 这里**故意不使用 KLF_ACTIVATE (0x1)**：它会把英文布局激活到当前线程，
/// 而 LoadKeyboardLayoutW 本身就已经把布局登记进系统输入法列表了，
/// 两者叠加就是 issue #10 里"启动后 Win+Space 多出 ENG"的直接原因。
/// KLF_NOTELLSHELL (0x80) 则可以阻止 Shell 收到 HSHELL_LANGUAGE 通知，
/// 避免任务栏输入法指示器被重新唤出来。
const KLF_NOTELLSHELL: u32 = 0x0080;

/// SendMessageTimeout 标志：目标线程挂死时立刻返回，不要把热键线程拖住
const SMTO_ABORTIFHUNG: u32 = 0x0002;
/// 与前台窗口 IME 通信的超时（毫秒）。粘贴路径对延迟敏感，宁可放弃保护也不能卡住。
const IME_CONTROL_TIMEOUT_MS: u32 = 80;

/// 英语主语言 ID（LANG_ENGLISH）
const LANG_ENGLISH: u16 = 0x09;
/// en-US 完整语言 ID
const LANGID_EN_US: u16 = 0x0409;

/// 本进程通过 LoadKeyboardLayoutW 主动加载的英文布局。
/// 0 表示从未主动加载过。退出时据此卸载，避免在用户的系统输入法列表里留下 ENG。
static SELF_LOADED_ENGLISH_HKL: AtomicIsize = AtomicIsize::new(0);

#[link(name = "user32")]
extern "system" {
    fn LoadKeyboardLayoutW(pwszklid: *const u16, flags: u32) -> HKL;
    fn UnloadKeyboardLayout(hkl: HKL) -> i32;
    fn GetKeyboardLayout(id_thread: u32) -> HKL;
    fn GetKeyboardLayoutList(n_buff: i32, lp_list: *mut HKL) -> i32;
    fn SendMessageTimeoutW(
        hwnd: isize,
        msg: u32,
        wparam: usize,
        lparam: isize,
        flags: u32,
        timeout: u32,
        result: *mut usize,
    ) -> isize;
}

#[link(name = "imm32")]
extern "system" {
    fn ImmGetDefaultIMEWnd(hwnd: isize) -> isize;
}

/// 取 HKL 低 16 位的输入语言 ID（高 16 位是设备句柄）
fn langid_of(hkl: HKL) -> u16 {
    (hkl as usize & 0xFFFF) as u16
}

/// 取主语言 ID（LANGID 的低 10 位）
fn primary_lang_of(hkl: HKL) -> u16 {
    langid_of(hkl) & 0x3FF
}

/// 在系统**已加载**的键盘布局里查找英文布局。
/// 纯查询，不会向系统新增任何布局，因此没有任何副作用。
fn find_loaded_english_layout() -> Option<HKL> {
    unsafe {
        let count = GetKeyboardLayoutList(0, std::ptr::null_mut());
        if count <= 0 {
            return None;
        }

        let mut list: Vec<HKL> = vec![0; count as usize];
        let filled = GetKeyboardLayoutList(count, list.as_mut_ptr());
        if filled <= 0 {
            return None;
        }
        list.truncate(filled as usize);

        // 优先精确匹配 en-US，其次退回任意英文变体（en-GB 等同样是纯字母布局）
        list.iter()
            .copied()
            .find(|hkl| langid_of(*hkl) == LANGID_EN_US)
            .or_else(|| {
                list.iter()
                    .copied()
                    .find(|hkl| primary_lang_of(*hkl) == LANG_ENGLISH)
            })
    }
}

/// 惰性获取英文布局 HKL。
///
/// 与旧实现的关键区别：**不在启动时调用**，且默认只复用系统已有的布局。
/// 只有 `allow_load = true`（`ime_protection = "layout-force"`）时才真正加载，
/// 且不带 KLF_ACTIVATE，退出时会由 [`unload_self_loaded_layout`] 卸载。
fn resolve_english_layout(allow_load: bool) -> Option<HKL> {
    // 1) 复用系统里已经装好的英文布局（绝大多数英文/多语言用户走这条路）
    if let Some(hkl) = find_loaded_english_layout() {
        return Some(hkl);
    }

    // 2) 用户没装英文布局：默认放弃切换，绝不擅自往系统里塞一个 ENG
    if !allow_load {
        return None;
    }

    // 3) 显式允许时才加载，并且只加载一次
    let cached = SELF_LOADED_ENGLISH_HKL.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached);
    }

    unsafe {
        let klid: Vec<u16> = "00000409\0".encode_utf16().collect();
        let hkl = LoadKeyboardLayoutW(klid.as_ptr(), KLF_NOTELLSHELL);
        if hkl == 0 {
            warn!("加载英文键盘布局失败，跳过输入法保护");
            return None;
        }
        info!("已惰性加载英文键盘布局 {:#x}（退出时会卸载）", hkl);
        SELF_LOADED_ENGLISH_HKL.store(hkl, Ordering::Relaxed);
        Some(hkl)
    }
}

/// 退出时卸载本进程主动加载过的英文布局，
/// 保证不会在用户的输入法切换列表里留下残留（issue #10）。
pub fn unload_self_loaded_layout() {
    let hkl = SELF_LOADED_ENGLISH_HKL.swap(0, Ordering::Relaxed);
    if hkl == 0 {
        return;
    }
    unsafe {
        if UnloadKeyboardLayout(hkl) == 0 {
            warn!("卸载英文键盘布局 {:#x} 失败", hkl);
        } else {
            info!("已卸载本进程加载的英文键盘布局 {:#x}", hkl);
        }
    }
}

/// 向前台窗口的默认 IME 窗口发送 WM_IME_CONTROL。
/// 使用带超时的 SendMessageTimeoutW，避免目标进程无响应时卡住粘贴。
unsafe fn send_ime_control(ime_wnd: isize, sub_command: usize, lparam: isize) -> Option<usize> {
    let mut result: usize = 0;
    let ok = SendMessageTimeoutW(
        ime_wnd,
        WM_IME_CONTROL,
        sub_command,
        lparam,
        SMTO_ABORTIFHUNG,
        IME_CONTROL_TIMEOUT_MS,
        &mut result,
    );
    if ok == 0 {
        None
    } else {
        Some(result)
    }
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

/// 恢复动作的延迟（毫秒）：必须晚于注入的 Ctrl+V 被目标窗口真正处理的时刻，
/// 否则输入法会在粘贴完成前就被切回去。
const RESTORE_DELAY_MS: u64 = 120;

/// [`ImeGuard`] 在析构时需要撤销的动作。
/// 只有真正改动过状态才会记录，"什么都没做"就绝不会去恢复。
enum ImeRestore {
    /// 未做任何改动，无需恢复
    Nothing,
    /// 需要把键盘布局切回原值
    Layout { hwnd: HWND, previous_hkl: HKL },
    /// 需要把 IME 输入状态重新打开
    OpenStatus { ime_wnd: isize },
}

/// 输入法保护器
///
/// 在粘贴前把前台窗口置为"直接输入英文"的状态，粘贴完成后异步恢复。
/// 具体手段由 [`ImeProtection`] 策略决定，详见该枚举的说明与 issue #10。
///
/// 设计原则：输入法保护是**尽力而为**的辅助措施。任何一步失败都安静降级成
/// "不保护"，绝不让输入法问题导致粘贴本身失败，也绝不擅自改动系统输入法列表。
pub struct ImeGuard {
    restore: ImeRestore,
}

impl ImeGuard {
    /// 按给定策略保护输入法状态
    pub fn new(policy: ImeProtection) -> Self {
        let restore = match policy {
            ImeProtection::Off => ImeRestore::Nothing,
            ImeProtection::Imm => Self::close_ime_open_status(),
            ImeProtection::Layout | ImeProtection::LayoutForce => {
                Self::switch_to_english_layout(policy.allows_loading_layout())
            }
        };

        Self { restore }
    }

    /// 关闭前台窗口的 IME 输入状态（即切到直接输入 / 英文模式）。
    ///
    /// 这是默认策略：只和目标窗口的 IME 窗口通信，**完全不触碰键盘布局**，
    /// 因此不会像旧实现那样在系统输入法列表里新增 `ENG`（issue #10）。
    fn close_ime_open_status() -> ImeRestore {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0 == 0 {
                return ImeRestore::Nothing;
            }

            let ime_wnd = ImmGetDefaultIMEWnd(hwnd.0);
            if ime_wnd == 0 {
                // 前台窗口没有关联 IME 窗口（纯英文环境常见），无需保护
                return ImeRestore::Nothing;
            }

            // 已经处于关闭（直接输入）状态时不做任何改动，也就不需要恢复
            match send_ime_control(ime_wnd, IMC_GETOPENSTATUS, 0) {
                Some(0) => return ImeRestore::Nothing,
                None => {
                    warn!("ImeGuard: 查询 IME 输入状态超时，跳过输入法保护");
                    return ImeRestore::Nothing;
                }
                Some(_) => {}
            }

            if send_ime_control(ime_wnd, IMC_SETOPENSTATUS, 0).is_none() {
                warn!("ImeGuard: 关闭 IME 输入状态超时，跳过输入法保护");
                return ImeRestore::Nothing;
            }

            info!("ImeGuard: 已关闭前台窗口 IME 输入状态");
            ImeRestore::OpenStatus { ime_wnd }
        }
    }

    /// 切换到英文键盘布局（兼容旧行为）。
    ///
    /// `allow_load` 为 false 时只复用系统里已经装好的英文布局，
    /// 找不到就直接放弃，绝不调用 LoadKeyboardLayoutW 往系统里新增布局。
    fn switch_to_english_layout(allow_load: bool) -> ImeRestore {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0 == 0 {
                return ImeRestore::Nothing;
            }

            let thread_id = GetWindowThreadProcessId(hwnd, None);
            if thread_id == 0 {
                return ImeRestore::Nothing;
            }

            let current_hkl = GetKeyboardLayout(thread_id);

            // 当前已经是英文布局，无需切换
            if primary_lang_of(current_hkl) == LANG_ENGLISH {
                return ImeRestore::Nothing;
            }

            let english_hkl = match resolve_english_layout(allow_load) {
                Some(hkl) => hkl,
                None => {
                    info!("ImeGuard: 系统未加载英文键盘布局，跳过切换（不会擅自添加 ENG）");
                    return ImeRestore::Nothing;
                }
            };

            if current_hkl == english_hkl {
                return ImeRestore::Nothing;
            }

            info!(
                "ImeGuard: 切换输入法 {:#x} -> {:#x}",
                current_hkl, english_hkl
            );
            let _ = PostMessageW(
                hwnd,
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(0),
                LPARAM(english_hkl),
            );
            std::thread::sleep(std::time::Duration::from_millis(60));

            ImeRestore::Layout {
                hwnd,
                previous_hkl: current_hkl,
            }
        }
    }
}

impl Drop for ImeGuard {
    /// 析构时在后台线程延迟恢复，既不阻塞热键线程，
    /// 也保证恢复发生在注入的 Ctrl+V 被目标窗口处理之后
    fn drop(&mut self) {
        match std::mem::replace(&mut self.restore, ImeRestore::Nothing) {
            ImeRestore::Nothing => {}

            ImeRestore::Layout { hwnd, previous_hkl } => {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(RESTORE_DELAY_MS));
                    unsafe {
                        if let Err(e) = PostMessageW(
                            hwnd,
                            WM_INPUTLANGCHANGEREQUEST,
                            WPARAM(0),
                            LPARAM(previous_hkl),
                        ) {
                            warn!("恢复输入法失败: {:?}", e);
                        } else {
                            info!("ImeGuard: 已恢复输入法 {:#x}", previous_hkl);
                        }
                    }
                });
            }

            ImeRestore::OpenStatus { ime_wnd } => {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(RESTORE_DELAY_MS));
                    unsafe {
                        if send_ime_control(ime_wnd, IMC_SETOPENSTATUS, 1).is_none() {
                            warn!("恢复 IME 输入状态超时");
                        } else {
                            info!("ImeGuard: 已恢复 IME 输入状态");
                        }
                    }
                });
            }
        }
    }
}

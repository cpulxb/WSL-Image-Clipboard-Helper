//! 判断 ssh 会话是否就在用户正要粘贴的那个终端 tab 里（issue #11）。
//!
//! 难点在 Windows Terminal：所有 tab 共用一个顶层窗口，前台窗口句柄分不出 tab，
//! UIA 也只给标题（多个 tab 同名很常见）。但每个 tab 有自己的控制台（OpenConsole），
//! ConPTY 会把控制台标题转发成 tab 标题，而活动 tab 的标题就是窗口标题。
//! 于是给前台 WT 窗口里每个控制台的标题临时追加不同个数的零宽空格（不可见），
//! 看窗口标题变成了哪一个，就知道活动 tab 是哪个控制台，随即还原。
//! ssh 进程再 attach 到它自己的控制台取控制台窗口句柄，两边一比即可。
//!
//! AttachConsole 直接在本进程内做（起 GUI 子进程会让鼠标闪"后台运行"光标）：
//! 每次只挂几微秒，前后保存 / 还原 std handles，避免其他线程的日志写进别人的 tab；全程串行。
//!
//! 判断不出活动 tab 时（profile 设了 suppressApplicationTitle、非 WT 终端等）不猜，
//! 退回旧行为：前台程序里的 ssh 会话都算候选。

use std::collections::HashSet;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GetConsoleTitleW, GetConsoleWindow, GetStdHandle,
    SetConsoleCtrlHandler, SetConsoleTitleW, SetStdHandle, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, GA_ROOTOWNER,
};

use crate::remote::ProcEntry;

/// 会话与前台（用户正要粘贴的地方）的关系
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// 就在用户正要粘贴的 tab 里
    Active,
    /// 在前台程序里，但分不出是不是当前 tab
    Maybe,
    /// 不在前台
    Inactive,
}

/// 用来定位会话所在控制台的 Windows 进程
pub enum Anchor {
    /// 与会话共用控制台的进程：Windows ssh 即其自身；WSL ssh 为同一 tab（WT_SESSION 相同）的 wsl.exe
    Process(u32),
    /// 对应不到具体进程（WSL 会话不在 WT 里、在 tmux 里等）：只能看前台程序下有没有 wsl.exe
    AnyWsl,
}

const PSEUDO_CONSOLE_CLASS: &str = "PseudoConsoleWindow";
const CONHOST_CLASS: &str = "ConsoleWindowClass";
const WT_CLASS: &str = "CASCADIA_HOSTING_WINDOW_CLASS";
/// 零宽空格：追加到标题里不可见，WT 会原样保留
const ZWSP: u16 = 0x200B;
/// 等 WT 把标记同步到窗口标题的最长时间（实测约 12ms）
const MARK_TIMEOUT: Duration = Duration::from_millis(200);
/// 标记被程序自己的标题刷新冲掉时（如 Claude Code 的 spinner），最多探测几轮
const MARK_ROUNDS: usize = 2;

/// AttachConsole 是进程级状态，同一时刻只能挂一个控制台
static PROBE_LOCK: Mutex<()> = Mutex::new(());
static CTRL_HANDLER: Once = Once::new();

/// 逐个判断会话与前台的关系，结果与 `anchors` 一一对应
pub fn classify(anchors: &[Anchor], procs: &[ProcEntry]) -> Vec<Focus> {
    let _guard = PROBE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0 == 0 {
            return vec![Focus::Maybe; anchors.len()];
        }
        let fg_pid = window_pid(fg);
        let gui_pids = gui_pids();
        let in_fg_app = |pid: u32| terminal_of(procs, &gui_pids, pid) == Some(fg_pid);

        // 每个会话：所在控制台窗口（取不到为 None）+ 是否位于前台程序里
        let facts: Vec<(Option<HWND>, bool)> = anchors
            .iter()
            .map(|anchor| match *anchor {
                Anchor::Process(pid) => {
                    let console = console_window_of(pid);
                    let hosted = match console {
                        Some(h) => {
                            let root = GetAncestor(h, GA_ROOTOWNER);
                            // WT 把各 tab 的伪控制台窗口 owner 设成自己的窗口；其他 ConPTY 终端
                            // （VS Code 等）不设 owner，只能按进程树看是不是前台程序开的
                            root == fg
                                || (root == h
                                    && class_name(h) == PSEUDO_CONSOLE_CLASS
                                    && in_fg_app(pid))
                        }
                        None => in_fg_app(pid),
                    };
                    (console, hosted)
                }
                Anchor::AnyWsl => (
                    None,
                    procs
                        .iter()
                        .any(|p| p.exe.eq_ignore_ascii_case("wsl.exe") && in_fg_app(p.pid)),
                ),
            })
            .collect();

        // 没有会话在前台程序里就不必探测，也就不去碰任何 tab 的标题
        let active = if !facts.iter().any(|&(_, hosted)| hosted) {
            None
        } else {
            match class_name(fg).as_str() {
                CONHOST_CLASS => Some(fg),
                WT_CLASS => active_wt_console(fg),
                _ => None,
            }
        };

        tracing::info!(foreground_pid = fg_pid, foreground_class = %class_name(fg),
            active_console = ?active, session_consoles = ?facts, "前台控制台探测结果");
        anchors
            .iter()
            .zip(facts)
            .map(|(anchor, (console, hosted))| match anchor {
                Anchor::Process(_) => decide(active, console, hosted),
                // 对应不到 tab：不猜，前台程序里有 WSL 就按旧行为算候选
                Anchor::AnyWsl if hosted => Focus::Maybe,
                Anchor::AnyWsl => Focus::Inactive,
            })
            .collect()
    }
}

/// `active`：确定了的活动控制台窗口；`console`：会话所在控制台窗口；`hosted`：会话是否在前台程序里
fn decide(active: Option<HWND>, console: Option<HWND>, hosted: bool) -> Focus {
    match active {
        Some(a) if console == Some(a) => Focus::Active,
        // 已确定用户在哪个 tab，其余会话都不在前台
        Some(_) => Focus::Inactive,
        None if hosted => Focus::Maybe,
        None => Focus::Inactive,
    }
}

/// 进程所在的终端程序：沿父进程链往上第一个有可见窗口的进程（含自身）。
/// 不能只看"前台进程是不是祖先"：explorer.exe 几乎是所有进程的祖先
fn terminal_of(procs: &[ProcEntry], gui_pids: &HashSet<u32>, mut pid: u32) -> Option<u32> {
    // 限制深度：父进程退出后 pid 可能被复用成环
    for _ in 0..32 {
        if gui_pids.contains(&pid) {
            return Some(pid);
        }
        let p = procs.iter().find(|p| p.pid == pid)?;
        if p.ppid == 0 || p.ppid == pid {
            return None;
        }
        pid = p.ppid;
    }
    None
}

/// 拥有可见顶层窗口的进程。控制台窗口不算：它报告的是控制台里的 shell，而不是终端程序
unsafe fn gui_pids() -> HashSet<u32> {
    top_level_windows()
        .into_iter()
        .filter(|&h| IsWindowVisible(h).as_bool())
        .filter(|&h| !matches!(class_name(h).as_str(), PSEUDO_CONSOLE_CLASS | CONHOST_CLASS))
        .map(|h| window_pid(h))
        .collect()
}

/// 找出前台 WT 窗口里活动 tab 的控制台窗口；判断不出返回 None
unsafe fn active_wt_console(wt: HWND) -> Option<HWND> {
    // 伪控制台窗口报告的进程是该控制台上的一个客户端（通常是 tab 的 shell），attach 它即可
    let tabs: Vec<(HWND, u32)> = top_level_windows()
        .into_iter()
        .filter(|&h| class_name(h) == PSEUDO_CONSOLE_CLASS && GetAncestor(h, GA_ROOTOWNER) == wt)
        .map(|h| (h, window_pid(h)))
        .collect();

    struct Mark {
        console: HWND,
        pid: u32,
        original: Vec<u16>,
        marked: Vec<u16>,
    }

    for _ in 0..MARK_ROUNDS {
        // 每个控制台追加不同个数的零宽空格，标题相同的 tab 也能区分
        let mut marks = Vec::new();
        for (i, &(console, pid)) in tabs.iter().enumerate() {
            let Some(_attached) = Attached::to(pid) else {
                continue;
            };
            if GetConsoleWindow() != console {
                continue;
            }
            let original = console_title();
            let mut marked = original.clone();
            marked.extend(std::iter::repeat(ZWSP).take(i + 1));
            if set_console_title(&marked) {
                marks.push(Mark {
                    console,
                    pid,
                    original,
                    marked,
                });
            }
        }
        if marks.is_empty() {
            return None;
        }

        let deadline = Instant::now() + MARK_TIMEOUT;
        let found = loop {
            let title = window_text(wt);
            if let Some(m) = marks.iter().find(|m| m.marked == title) {
                break Some(m.console);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(2));
        };

        // 还原；标题已被程序自己改掉的不动（它的新标题更准）
        let mut overwritten = false;
        for m in &marks {
            let Some(_attached) = Attached::to(m.pid) else {
                continue;
            };
            if console_title() == m.marked {
                set_console_title(&m.original);
            } else {
                overwritten = true;
            }
        }
        if found.is_some() || !overwritten {
            return found;
        }
    }
    None
}

/// 挂到某个进程的控制台上，drop 时 FreeConsole
struct Attached;

impl Attached {
    unsafe fn to(pid: u32) -> Option<Self> {
        let kinds = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
        let saved = kinds.map(|k| GetStdHandle(k).unwrap_or_default());
        let attached = AttachConsole(pid).is_ok();
        // AttachConsole 会把本进程的 std handles 指向该控制台；立刻还原，
        // 否则其他线程此刻打的日志会写进别人的 tab
        for (kind, handle) in kinds.into_iter().zip(saved) {
            let _ = SetStdHandle(kind, handle);
        }
        if !attached {
            return None;
        }
        // 挂着的这几微秒里用户恰好在该 tab 按 Ctrl+C，默认处理会让本进程退出
        CTRL_HANDLER.call_once(|| {
            let _ = SetConsoleCtrlHandler(Some(ignore_ctrl_c), true);
        });
        Some(Attached)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeConsole();
        }
    }
}

unsafe extern "system" fn ignore_ctrl_c(ctrl_type: u32) -> BOOL {
    BOOL::from(ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT)
}

unsafe fn console_window_of(pid: u32) -> Option<HWND> {
    let _attached = Attached::to(pid)?;
    let hwnd = GetConsoleWindow();
    (hwnd.0 != 0).then_some(hwnd)
}

unsafe fn console_title() -> Vec<u16> {
    let mut buf = vec![0u16; 1024];
    let len = GetConsoleTitleW(&mut buf) as usize;
    buf.truncate(len.min(buf.len()));
    buf
}

unsafe fn set_console_title(title: &[u16]) -> bool {
    let mut z = title.to_vec();
    z.push(0);
    SetConsoleTitleW(PCWSTR::from_raw(z.as_ptr())).is_ok()
}

unsafe fn window_text(hwnd: HWND) -> Vec<u16> {
    let mut buf = vec![0u16; 1024];
    let len = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    buf.truncate(len);
    buf
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = GetClassNameW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..len])
}

unsafe fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    pid
}

unsafe fn top_level_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        (*(lparam.0 as *mut Vec<HWND>)).push(hwnd);
        BOOL::from(true)
    }
    let mut windows: Vec<HWND> = Vec::new();
    let _ = EnumWindows(
        Some(collect),
        LPARAM(&mut windows as *mut Vec<HWND> as isize),
    );
    windows
}

/// 读取另一个进程的环境变量（读它 PEB 里的环境块）。只支持 64 位进程，本程序也只发布 x64
pub fn env_var(pid: u32, name: &str) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?;
        let value = read_env_var(handle, name);
        let _ = CloseHandle(handle);
        value
    }
}

unsafe fn process_parameters(handle: HANDLE) -> Option<usize> {
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};

    /// PROCESS_BASIC_INFORMATION（x64 布局）
    #[repr(C)]
    #[derive(Default)]
    struct BasicInfo {
        exit_status: i32,
        peb: usize,
        affinity_mask: usize,
        base_priority: i32,
        pid: usize,
        parent_pid: usize,
    }
    /// x64：PEB.ProcessParameters
    const PEB_PROCESS_PARAMETERS: usize = 0x20;

    let mut info = BasicInfo::default();
    let mut len = 0u32;
    let status = NtQueryInformationProcess(
        handle,
        ProcessBasicInformation,
        (&mut info as *mut BasicInfo).cast(),
        std::mem::size_of::<BasicInfo>() as u32,
        &mut len,
    );
    if !status.is_ok() || info.peb == 0 {
        return None;
    }
    read_remote(handle, info.peb + PEB_PROCESS_PARAMETERS)
}

unsafe fn read_env_var(handle: HANDLE, name: &str) -> Option<String> {
    const PARAMS_ENVIRONMENT: usize = 0x80;
    const PARAMS_ENVIRONMENT_SIZE: usize = 0x3F0;
    const MAX_ENV_BYTES: usize = 256 * 1024;
    let params = process_parameters(handle)?;
    let env: usize = read_remote(handle, params + PARAMS_ENVIRONMENT)?;
    let size: usize = read_remote(handle, params + PARAMS_ENVIRONMENT_SIZE)?;
    if env == 0 {
        return None;
    }

    let mut block = vec![0u16; size.min(MAX_ENV_BYTES) / 2];
    read_remote_into(handle, env, &mut block)?;
    let prefix: Vec<u16> = format!("{}=", name).encode_utf16().collect();
    block
        .split(|&c| c == 0)
        .find(|entry| entry.starts_with(&prefix))
        .map(|entry| String::from_utf16_lossy(&entry[prefix.len()..]))
}

/// Preserve the launching directory for relative -i/-F and ProxyCommand paths.
pub fn process_directory(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    #[repr(C)]
    #[derive(Default)]
    struct UnicodeString {
        length: u16,
        maximum: u16,
        buffer: usize,
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?;
        let value = (|| {
            let params = process_parameters(handle)?;
            let directory: UnicodeString = read_remote(handle, params + 0x38)?;
            if directory.length == 0
                || directory.length > directory.maximum
                || directory.buffer == 0
            {
                return None;
            }
            let mut text = vec![0u16; directory.length as usize / 2];
            read_remote_into(handle, directory.buffer, &mut text)?;
            Some(String::from_utf16_lossy(&text))
        })();
        let _ = CloseHandle(handle);
        value
    }
}

unsafe fn read_remote<T: Default>(handle: HANDLE, addr: usize) -> Option<T> {
    let mut value = T::default();
    read_remote_into(handle, addr, std::slice::from_mut(&mut value))?;
    Some(value)
}

unsafe fn read_remote_into<T>(handle: HANDLE, addr: usize, buf: &mut [T]) -> Option<()> {
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    ReadProcessMemory(
        handle,
        addr as *const _,
        buf.as_mut_ptr().cast(),
        std::mem::size_of_val(buf),
        None,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_current_process_working_directory() {
        let current = std::env::current_dir().unwrap();
        assert_eq!(
            std::path::PathBuf::from(process_directory(std::process::id()).unwrap()),
            current
        );
    }

    fn proc(pid: u32, ppid: u32) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            exe: String::new(),
        }
    }

    #[test]
    fn terminal_is_nearest_ancestor_with_a_window() {
        // 10(explorer) → 20(WindowsTerminal) → 30(pwsh) → 40(ssh)
        let procs = [proc(10, 1), proc(20, 10), proc(30, 20), proc(40, 30)];
        let gui: HashSet<u32> = [10, 20].into();
        // 桌面 / 资源管理器在前台时，不能因为 explorer 是祖先就算进来
        assert_eq!(terminal_of(&procs, &gui, 40), Some(20));
        assert_eq!(terminal_of(&procs, &gui, 20), Some(20));
        assert_eq!(terminal_of(&procs, &HashSet::new(), 40), None);
        // pid 复用成环也能停下
        let cycle = [proc(5, 6), proc(6, 5)];
        assert_eq!(terminal_of(&cycle, &gui, 5), None);
    }

    #[test]
    fn known_active_tab_excludes_other_sessions() {
        let (tab_a, tab_b) = (Some(HWND(1)), Some(HWND(2)));
        assert_eq!(decide(tab_a, tab_a, true), Focus::Active);
        // 同一 WT 窗口的另一个 tab：即使在前台程序里也不算
        assert_eq!(decide(tab_a, tab_b, true), Focus::Inactive);
        assert_eq!(decide(tab_a, None, true), Focus::Inactive);
    }

    #[test]
    fn unknown_active_tab_falls_back_to_foreground_app() {
        assert_eq!(decide(None, Some(HWND(2)), true), Focus::Maybe);
        assert_eq!(decide(None, Some(HWND(2)), false), Focus::Inactive);
    }
}

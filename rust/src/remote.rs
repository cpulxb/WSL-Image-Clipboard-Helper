//! 远程粘贴（issue #11）：自动发现当前打开的 ssh 会话，把本地文件用同一份 ssh 参数
//! 上传到远程主机，返回远程路径；按需认证并保持上传连接。
//!
//! 发现：枚举 Windows 侧的 `ssh.exe` 进程（读取命令行）和 WSL 默认发行版里的 `ssh`
//! 进程，只保留交互式登录会话（排除 `-N` 隧道、`-T`/VS Code Remote、`-W`/`-O` 等）。
//! 只上传到用户正要粘贴的那个终端 tab 里的会话（见 [`crate::foreground`]）：
//! 同一个 Windows Terminal 里本地 tab 和 ssh tab 并存时，在本地 tab 按热键仍粘本地路径。
//! 托盘菜单可手动固定某个会话。
//!
//! 传输：保持专用的非交互 SSH channel，通过带长度的协议连续上传文件。
//! 认证：先尝试现有密钥 / agent / WSL ControlPath；需要时用原生 askpass 窗口。
//! 密码不落盘；退出只关闭已有连接，由远程 shell 清理临时截图。

use anyhow::Result;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::foreground::{self, Anchor, Focus};

/// 给用户输入密码/口令的时间；连接建立后另有传输超时。
const AUTH_TIMEOUT: Duration = Duration::from_secs(180);
/// 首次静默认证（包含跳板）的最长等待。
const SILENT_AUTH_TIMEOUT: Duration = Duration::from_secs(20);
/// ssh 建连超时（秒），主机不可达时尽快失败
const CONNECT_TIMEOUT_SECS: u32 = 5;
/// CREATE_NO_WINDOW：本程序是无控制台的 GUI 进程，
/// 不加该标志启动 ssh.exe / wsl.exe 会闪出一个黑色控制台窗口
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// ssh 客户端来自哪一侧：别名、密钥、known_hosts 都只在那一侧存在，
/// 上传必须复用发现到会话的那一侧
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshBackend {
    /// Windows 侧 `ssh.exe`（自带 OpenSSH 或 Git 等附带的）
    Windows,
    /// WSL 默认发行版里的 `ssh`，通过 `wsl.exe -e ssh` 调用
    Wsl,
}

impl SshBackend {
    pub fn display_name(&self) -> &'static str {
        match self {
            SshBackend::Windows => "Windows ssh",
            SshBackend::Wsl => "WSL ssh",
        }
    }
}

/// 一个正在运行的交互式 ssh 会话
#[derive(Debug, Clone)]
pub struct SshSession {
    pub backend: SshBackend,
    pub pid: u32,
    /// ssh 的 destination 参数（`user@host` / 别名 / `ssh://...`），仅用于显示
    pub destination: String,
    /// Windows 侧：发现到的 ssh.exe 完整路径，复用同一个客户端及其配置；WSL 侧固定 `ssh`
    program: PathBuf,
    /// 从原会话复用的 ssh 参数（选项 + destination），已剔除远程命令与转发 / tty 类选项
    args: Vec<String>,
    /// 进程启动时间（unix 秒），用于"最近打开的会话优先"
    started: u64,
    /// WSL 侧：会话所在 Windows Terminal tab 的 WT_SESSION，用来对应到该 tab 的 wsl.exe
    wt_session: Option<String>,
    wsl_context: Option<(String, String)>,
    cwd: Option<String>,
    auth_sock: Option<String>,
}

impl SshSession {
    fn wsl_prefix(&self) -> Vec<String> {
        match &self.wsl_context {
            Some((distro, user)) => vec!["-d".into(), distro.clone(), "-u".into(), user.clone()],
            None => Vec::new(),
        }
    }

    pub fn label(&self) -> String {
        format!("{}  ({})", self.destination, self.backend.display_name())
    }

    /// 是否指向同一目标。忽略 pid：同一个 tab 断线重连后仍视为同一目标
    pub fn same_target(&self, other: &SshSession) -> bool {
        self.backend == other.backend
            && self.program == other.program
            && self.args == other.args
            && self.wsl_context == other.wsl_context
            && self.cwd == other.cwd
            && self.auth_sock == other.auth_sock
    }
}

/// 枚举当前所有交互式 ssh 会话，最近打开的排在前面
pub fn discover_sessions() -> Vec<SshSession> {
    discover_from(&process_snapshot())
}

fn discover_from(procs: &[ProcEntry]) -> Vec<SshSession> {
    let mut sessions = discover_windows(procs);
    sessions.extend(discover_wsl());
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started));
    sessions
}

/// 远程粘贴模式：Off / Auto 对应配置项 `remote_paste`，Pinned 由托盘菜单临时指定
#[derive(Debug, Clone)]
pub enum RemoteMode {
    /// 始终粘贴本地 /mnt 路径
    Off,
    /// 前台 tab 里有 ssh 会话就上传到它，否则粘贴本地路径
    Auto,
    /// 固定上传到指定会话，不看前台；该会话已关闭时退回 Auto 的行为
    Pinned(Box<SshSession>),
}

impl RemoteMode {
    pub fn from_config(remote_paste: bool) -> Self {
        if remote_paste {
            RemoteMode::Auto
        } else {
            RemoteMode::Off
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            RemoteMode::Off => "关闭".to_string(),
            RemoteMode::Auto => "自动".to_string(),
            RemoteMode::Pinned(s) => s.label(),
        }
    }

    /// 按当前模式解析这次粘贴要用的 ssh 会话；None = 粘贴本地路径。
    /// 会枚举进程（可能启动 wsl.exe）、探测前台 tab，请在阻塞线程上调用
    pub fn resolve(&self) -> Option<SshSession> {
        if matches!(self, RemoteMode::Off) {
            return None;
        }
        let procs = process_snapshot();
        let sessions = discover_from(&procs);
        if let RemoteMode::Pinned(pinned) = self {
            if let Some(s) = sessions.iter().find(|s| s.same_target(pinned)) {
                return Some(s.clone());
            }
            warn!("指定的 ssh 会话已关闭，退回自动选择: {}", pinned.label());
        }
        if sessions.is_empty() {
            info!("没有打开的 ssh 会话，粘贴本地路径");
            return None;
        }
        let anchors = anchors_of(&sessions, &procs);
        let focus = foreground::classify(&anchors, &procs);
        for ((session, anchor), focus) in sessions.iter().zip(&anchors).zip(&focus) {
            let anchor_pid = match anchor {
                Anchor::Process(pid) => Some(*pid),
                Anchor::AnyWsl => None,
            };
            info!(
                ssh_pid = session.pid,
                ?anchor_pid,
                ?focus,
                "SSH 会话与前台 tab 的匹配结果"
            );
        }
        let chosen = pick(sessions, &focus);
        match &chosen {
            Some((s, Focus::Active)) => {
                info!("前台 tab 中的 ssh 会话: {} (pid {})", s.label(), s.pid)
            }
            Some((s, _)) => info!(
                "无法确定前台 tab，使用前台程序里最近打开的 ssh 会话: {} (pid {})",
                s.label(),
                s.pid
            ),
            None => info!("前台 tab 里没有 ssh 会话，粘贴本地路径"),
        }
        chosen.map(|(s, _)| s)
    }
}

/// 优先前台 tab 里的会话；分不出 tab 时退回前台程序里最近打开的会话；都没有就粘本地路径。
/// `sessions` 已按最近打开排序，与 `focus` 一一对应
fn pick(sessions: Vec<SshSession>, focus: &[Focus]) -> Option<(SshSession, Focus)> {
    let (idx, &f) = [Focus::Active, Focus::Maybe]
        .iter()
        .find_map(|want| focus.iter().enumerate().find(|(_, f)| *f == want))?;
    sessions.into_iter().nth(idx).map(|s| (s, f))
}

/// 为每个会话找一个与它共用控制台的 Windows 进程，用于判断它在哪个 tab
fn anchors_of(sessions: &[SshSession], procs: &[ProcEntry]) -> Vec<Anchor> {
    // 只有存在 WSL 会话时才去读各 wsl.exe 的 WT_SESSION
    let mut wsl_tabs: Option<Vec<(u32, String)>> = None;
    sessions
        .iter()
        .map(|s| match (s.backend, &s.wt_session) {
            (SshBackend::Windows, _) => Anchor::Process(s.pid),
            (SshBackend::Wsl, Some(wt)) => {
                let tabs = wsl_tabs.get_or_insert_with(|| {
                    procs
                        .iter()
                        .filter(|p| p.exe.eq_ignore_ascii_case("wsl.exe"))
                        .filter_map(|p| Some((p.pid, foreground::env_var(p.pid, "WT_SESSION")?)))
                        .collect()
                });
                tabs.iter()
                    .find(|(_, t)| t == wt)
                    .map_or(Anchor::AnyWsl, |&(pid, _)| Anchor::Process(pid))
            }
            (SshBackend::Wsl, None) => Anchor::AnyWsl,
        })
        .collect()
}

/// 进程快照中的一项（发现 ssh.exe、判断前台时查进程树都用它）
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub exe: String,
}

pub fn process_snapshot() -> Vec<ProcEntry> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut procs = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return procs;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                procs.push(ProcEntry {
                    pid: entry.th32ProcessID,
                    ppid: entry.th32ParentProcessID,
                    exe: utf16_until_nul(&entry.szExeFile),
                });
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    procs
}

/// Windows 侧：在进程快照里找到 ssh.exe，读它的命令行 / 映像路径 / 启动时间
fn discover_windows(procs: &[ProcEntry]) -> Vec<SshSession> {
    procs
        .iter()
        .filter(|p| p.exe.eq_ignore_ascii_case("ssh.exe"))
        .filter_map(|p| unsafe { inspect_windows_process(p.pid) })
        .collect()
}

/// 读取一个 ssh.exe 进程的命令行等信息；非交互式会话或读取失败返回 None
unsafe fn inspect_windows_process(pid: u32) -> Option<SshSession> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows::Win32::Foundation::{CloseHandle, LocalFree, FILETIME, HLOCAL, UNICODE_STRING};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Shell::CommandLineToArgvW;

    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

    let result = (|| {
        // ProcessCommandLineInformation（Win 8.1+）：只需 QUERY_LIMITED 权限，
        // 不用读对方 PEB。先问长度，再取内容
        let mut len: u32 = 0;
        let _ = NtQueryInformationProcess(
            handle,
            ProcessCommandLineInformation,
            std::ptr::null_mut(),
            0,
            &mut len,
        );
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let status = NtQueryInformationProcess(
            handle,
            ProcessCommandLineInformation,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        );
        if !status.is_ok() {
            return None;
        }
        let ustr = &*(buf.as_ptr() as *const UNICODE_STRING);
        if ustr.Buffer.is_null() {
            return None;
        }
        let cmdline = std::slice::from_raw_parts(ustr.Buffer.0, (ustr.Length / 2) as usize);
        let mut cmdline_z: Vec<u16> = cmdline.to_vec();
        cmdline_z.push(0);

        // 按 Windows 规则切分命令行；argv[0] 是程序本身
        let mut argc: i32 = 0;
        let argv_ptr = CommandLineToArgvW(PCWSTR::from_raw(cmdline_z.as_ptr()), &mut argc);
        if argv_ptr.is_null() {
            return None;
        }
        let argv: Vec<String> = (0..argc.max(0) as usize)
            .map(|i| (*argv_ptr.add(i)).to_string().unwrap_or_default())
            .collect();
        let _ = LocalFree(HLOCAL(argv_ptr as *mut _));

        let mut exe_buf = [0u16; 1024];
        let mut exe_len = exe_buf.len() as u32;
        let program = if QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(exe_buf.as_mut_ptr()),
            &mut exe_len,
        )
        .is_ok()
        {
            PathBuf::from(String::from_utf16_lossy(&exe_buf[..exe_len as usize]))
        } else {
            PathBuf::from("ssh.exe")
        };

        let mut created = FILETIME::default();
        let mut ignored = [FILETIME::default(); 3];
        let started = if GetProcessTimes(
            handle,
            &mut created,
            &mut ignored[0],
            &mut ignored[1],
            &mut ignored[2],
        )
        .is_ok()
        {
            filetime_to_unix(created)
        } else {
            0
        };

        let mut session =
            session_from_argv(SshBackend::Windows, pid, program, argv.get(1..)?, started)?;
        session.auth_sock = foreground::env_var(pid, "SSH_AUTH_SOCK");
        session.cwd = foreground::process_directory(pid);
        Some(session)
    })();

    let _ = CloseHandle(handle);
    result
}

fn utf16_until_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn filetime_to_unix(ft: windows::Win32::Foundation::FILETIME) -> u64 {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    // FILETIME 以 100ns 计，从 1601-01-01 起算
    (ticks / 10_000_000).saturating_sub(11_644_473_600)
}

/// WSL 进程记录以 RS 分隔，环境字段和 argv 以 NUL 分隔，保留参数中的空格。
/// 在 tmux / screen 里的 ssh 不报 WT_SESSION：它继承的是 tmux 服务端启动时那个 tab 的值，
/// 不代表用户现在从哪个 tab 看它
const WSL_PS_SCRIPT: &str = r#"uid=$(id -u); user=$(id -un)
ps -o uid=,pid=,etimes= -C ssh | while read -r owner pid elapsed; do
    [ "$owner" = "$uid" ] || continue
    [ -r "/proc/$pid/cmdline" ] || continue
    e=$(tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null) || continue
    wt=$(printf '%s\n' "$e" | sed -n 's/^WT_SESSION=//p')
    printf '%s\n' "$e" | grep -q -e '^TMUX=' -e '^STY=' && wt=
    sock=$(printf '%s\n' "$e" | sed -n 's/^SSH_AUTH_SOCK=//p')
    cwd=$(readlink "/proc/$pid/cwd") || continue
    exe=$(readlink "/proc/$pid/exe") || continue
    printf '\036%s\0%s\0%s\0%s\0%s\0%s\0%s\0%s\0' "$pid" "$wt" "$elapsed" "$WSL_DISTRO_NAME" "$user" "$cwd" "$sock" "$exe"
    cat "/proc/$pid/cmdline"
done"#;

/// WSL 侧：默认发行版正在运行时，用 ps 列出其中的 ssh 进程
fn discover_wsl() -> Vec<SshSession> {
    // 发行版没在跑说明里面不可能有 ssh 会话；直接 `wsl.exe -e` 会把它启动起来，白等几秒
    if !wsl_is_running() {
        return Vec::new();
    }
    let output = std_command("wsl.exe")
        .args(["-e", "sh", "-c", WSL_PS_SCRIPT])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            parse_ps_output(&String::from_utf8_lossy(&out.stdout), unix_now())
        }
        _ => Vec::new(),
    }
}

fn wsl_is_running() -> bool {
    let Ok(out) = std_command("wsl.exe")
        .args(["--list", "--running", "--quiet"])
        .output()
    else {
        return false;
    };
    // wsl.exe 自身的输出是 UTF-16LE
    let wide: Vec<u16> = out
        .stdout
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    !String::from_utf16_lossy(&wide).trim().is_empty()
}

/// NUL-separated argv preserves spaces in identity/config paths. Record separator is RS.
fn parse_ps_output(text: &str, now: u64) -> Vec<SshSession> {
    text.split('\u{1e}')
        .filter_map(|record| {
            let mut fields = record.split('\0');
            let pid = fields.next()?.parse().ok()?;
            let wt = fields.next()?;
            let elapsed: u64 = fields.next()?.parse().ok()?;
            let distro = fields.next()?.to_string();
            let user = fields.next()?.to_string();
            let cwd = fields.next()?.to_string();
            let sock = fields.next()?;
            let program = PathBuf::from(fields.next()?);
            fields.next()?; // argv[0]
            let argv: Vec<String> = fields
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let mut session = session_from_argv(
                SshBackend::Wsl,
                pid,
                program,
                &argv,
                now.saturating_sub(elapsed),
            )?;
            session.wt_session = (!wt.is_empty()).then(|| wt.to_string());
            session.wsl_context = Some((distro, user));
            session.cwd = Some(cwd);
            session.auth_sock = (!sock.is_empty()).then(|| sock.to_string());
            Some(session)
        })
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn std_command(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn session_from_argv(
    backend: SshBackend,
    pid: u32,
    program: PathBuf,
    argv: &[String],
    started: u64,
) -> Option<SshSession> {
    let (args, destination) = parse_ssh_args(argv)?;
    Some(SshSession {
        backend,
        pid,
        destination,
        program,
        args,
        started,
        wt_session: None,
        wsl_context: None,
        cwd: None,
        auth_sock: None,
    })
}

/// 带参数的 ssh 选项字母（见 `ssh` 用法一行）
const SSH_OPTS_WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";
/// 不带参数的 ssh 开关字母
const SSH_FLAGS: &str = "46AaCfGgKkMNnqsTtVvXxYy";
/// 上传时复用的带参数选项：认证 / 路由 / 配置相关。
/// 端口转发（-D/-L/-R/-w）、日志（-E）、转义字符（-e）不复用：再开一次只会报端口占用
const OPTS_REUSED: &str = "BbcFIiJlmoPpS";
/// 上传时复用的开关：地址族、压缩、GSSAPI 认证等。
/// 不复用 -A/-X/-Y（转发）、-n（stdin 接 /dev/null，会让上传空文件）、-t/-q/-v 等
const FLAGS_REUSED: &str = "46CKkagx";

/// 从原会话的 ssh 参数（不含 argv[0]）里提炼上传用的参数列表和 destination。
/// 返回 None 表示这不是一个交互式登录会话：
/// `-N`（纯隧道）、`-f`（后台）、`-T`（无 tty，如 VS Code Remote-SSH / git）、
/// `-W`/`-O`/`-G`/`-Q`/`-V`，以及"带远程命令但没有 -t"（脚本、git 等）
fn parse_ssh_args(argv: &[String]) -> Option<(Vec<String>, String)> {
    let mut kept: Vec<String> = Vec::new();
    let mut destination: Option<String> = None;
    let mut has_command = false;
    let mut wants_tty = false;

    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if destination.is_some() {
            has_command = true;
            break;
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) else {
            destination = Some(arg.clone());
            i += 1;
            continue;
        };
        let chars: Vec<char> = cluster.chars().collect();
        let mut j = 0;
        while j < chars.len() {
            let c = chars[j];
            if SSH_OPTS_WITH_ARG.contains(c) {
                // 参数可以紧跟在字母后（-p22）或作为下一个 token（-p 22）
                let value = if j + 1 < chars.len() {
                    chars[j + 1..].iter().collect::<String>()
                } else {
                    i += 1;
                    argv.get(i)?.clone()
                };
                match c {
                    'O' | 'Q' | 'W' => return None,
                    _ if OPTS_REUSED.contains(c) => {
                        kept.push(format!("-{}", c));
                        kept.push(value);
                    }
                    _ => {}
                }
                break;
            } else if SSH_FLAGS.contains(c) {
                match c {
                    'N' | 'f' | 'T' | 'G' | 'V' => return None,
                    't' => wants_tty = true,
                    _ if FLAGS_REUSED.contains(c) => kept.push(format!("-{}", c)),
                    _ => {}
                }
                j += 1;
            } else {
                // 不认识的选项：保守起见不当作会话
                return None;
            }
        }
        i += 1;
    }

    let destination = destination?;
    if has_command && !wants_tty {
        return None;
    }
    kept.push(destination.clone());
    Some((kept, destination))
}

struct LiveConnection {
    session: SshSession,
    transport: crate::remote_transport::Connection,
}

pub struct RemoteUploader {
    connections: Mutex<Vec<LiveConnection>>,
}

impl RemoteUploader {
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(Vec::new()),
        }
    }

    pub async fn upload(
        &self,
        session: &SshSession,
        local: &Path,
        temp_image: bool,
    ) -> Result<String> {
        let mut pool = self.connections.lock().await;
        let existing = pool.iter().position(|c| c.session.same_target(session));
        let index = if let Some(i) = existing {
            if pool[i].transport.healthy().await {
                Some(i)
            } else {
                pool.remove(i);
                None
            }
        } else {
            None
        };
        let i = match index {
            Some(i) => i,
            None => {
                let transport = connect(session).await?;
                pool.push(LiveConnection {
                    session: session.clone(),
                    transport,
                });
                pool.len() - 1
            }
        };
        let result = pool[i].transport.upload(local, temp_image).await;
        // A failed/partial transfer must never leave a desynchronized stream in the pool.
        if result.is_err() {
            pool.remove(i);
        }
        result
    }

    /// Only close existing channels. Never establish a connection or ask for passwords at exit.
    pub async fn cleanup_on_exit(&self) {
        let connections = std::mem::take(&mut *self.connections.lock().await);
        let mut tasks = tokio::task::JoinSet::new();
        for mut c in connections {
            tasks.spawn(async move {
                c.transport.close().await;
            });
        }
        while tasks.join_next().await.is_some() {}
    }
}

async fn connect(session: &SshSession) -> Result<crate::remote_transport::Connection> {
    match connect_attempt(session, true).await {
        Ok(c) => Ok(c),
        Err(e) if authentication_needed(&format!("{e:#}")) => {
            info!("SSH 需要认证，启用密码/密钥口令窗口: {}", session.label());
            connect_attempt(session, false).await
        }
        Err(e) => Err(e),
    }
}

fn authentication_needed(error: &str) -> bool {
    // Network failures and changed host keys must not trigger misleading password dialogs.
    !error.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        && (error.contains("Permission denied")
            || error.contains("Host key verification failed")
            || error.contains("Too many authentication failures"))
}

async fn connect_attempt(
    session: &SshSession,
    silent: bool,
) -> Result<crate::remote_transport::Connection> {
    let token = format!("WCH{:032x}", windows::core::GUID::new()?.to_u128());
    let worker = format!(
        "sh -c {} sh {}",
        sh_quote(crate::remote_transport::WORKER),
        sh_quote(&token)
    );
    let mut auth = crate::ssh_auth::Askpass::new(session.label(), silent)?;
    let options = ssh_options(session, &worker, silent);
    let mut cmd = match session.backend {
        SshBackend::Windows => {
            let mut cmd = Command::new(&session.program);
            cmd.args(options);
            auth.windows_env(&mut cmd);
            if let Some(sock) = &session.auth_sock {
                cmd.env("SSH_AUTH_SOCK", sock);
            } else {
                cmd.env_remove("SSH_AUTH_SOCK");
            }
            if let Some(cwd) = &session.cwd {
                cmd.current_dir(cwd);
            }
            cmd
        }
        SshBackend::Wsl => {
            let prefix = session.wsl_prefix();
            // Existing agent/multiplexed sessions also work with Windows interop disabled.
            let shim = if silent {
                "/bin/false".to_string()
            } else {
                auth.wsl_script(&prefix).await?
            };
            let mut cmd = Command::new("wsl.exe");
            cmd.args(prefix)
                .args([
                    "-e",
                    "sh",
                    "-c",
                    "cd -- \"$1\" && shift && exec env \"$@\"",
                    "sh",
                ])
                .arg(session.cwd.as_deref().unwrap_or("."))
                .arg(format!(
                    "SSH_AUTH_SOCK={}",
                    session.auth_sock.as_deref().unwrap_or("")
                ))
                .arg(format!("SSH_ASKPASS={shim}"))
                .args(["SSH_ASKPASS_REQUIRE=force", "DISPLAY=wsl-clipboard:0"])
                .arg(&session.program)
                .args(options);
            cmd
        }
    };
    cmd.creation_flags(CREATE_NO_WINDOW);
    crate::remote_transport::Connection::start(
        cmd,
        token,
        if silent {
            SILENT_AUTH_TIMEOUT
        } else {
            AUTH_TIMEOUT
        },
    )
    .await
}

fn ssh_options(session: &SshSession, remote_cmd: &str, silent: bool) -> Vec<String> {
    // First value wins. Preserve routing/authentication and existing ControlPath, while
    // preventing tty allocation, extra forwards, and user LocalCommand side effects.
    let mut args = vec!["-T".to_string()];
    for option in [
        format!("BatchMode={}", if silent { "yes" } else { "no" }),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "LogLevel=ERROR".into(),
        "NumberOfPasswordPrompts=1".into(),
        "ServerAliveInterval=15".into(),
        "ServerAliveCountMax=2".into(),
        "RequestTTY=no".into(),
        "RemoteCommand=none".into(),
        "ClearAllForwardings=yes".into(),
        "PermitLocalCommand=no".into(),
        "ForwardAgent=no".into(),
        "ForwardX11=no".into(),
        "ControlMaster=no".into(),
    ] {
        args.push("-o".into());
        args.push(option);
    }
    args.extend(session.args.iter().cloned());
    args.push(remote_cmd.to_string());
    args
}

fn sh_quote(s: &str) -> String {
    crate::ssh_auth::quote(s)
}

#[cfg(test)]
include!("../tests/support/remote_fixture.rs");

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("a'b c"), "'a'\\''b c'");
    }

    #[test]
    fn plain_login_is_a_session() {
        let (args, dest) = parse_ssh_args(&argv("root@1.2.3.4")).unwrap();
        assert_eq!(dest, "root@1.2.3.4");
        assert_eq!(args, ["root@1.2.3.4"]);
    }

    #[test]
    fn auth_and_route_options_are_reused_forwarding_is_dropped() {
        let (args, dest) = parse_ssh_args(&argv(
            "-p2222 -i C:\\k\\id -J jump -L 8080:localhost:80 -D 1080 -A -C -o ServerAliveInterval=30 dev",
        ))
        .unwrap();
        assert_eq!(dest, "dev");
        assert_eq!(
            args,
            [
                "-p",
                "2222",
                "-i",
                "C:\\k\\id",
                "-J",
                "jump",
                "-C",
                "-o",
                "ServerAliveInterval=30",
                "dev"
            ]
        );
    }

    #[test]
    fn non_interactive_invocations_are_ignored() {
        // 纯隧道
        assert!(parse_ssh_args(&argv("-N -L 5432:db:5432 dev")).is_none());
        // VS Code Remote-SSH
        assert!(parse_ssh_args(&argv("-T -D 51234 -o ConnectTimeout=15 dev bash")).is_none());
        // git / 脚本：带远程命令但没有 -t
        assert!(parse_ssh_args(&argv("git@github.com git-upload-pack repo")).is_none());
        // 本工具自己的上传进程
        assert!(parse_ssh_args(&argv("-o BatchMode=yes dev mkdir -p x && cat > y")).is_none());
        // 控制命令 / 版本 / 配置查询
        assert!(parse_ssh_args(&argv("-O check dev")).is_none());
        assert!(parse_ssh_args(&argv("-V")).is_none());
        assert!(parse_ssh_args(&argv("-G dev")).is_none());
        assert!(parse_ssh_args(&argv("-W host:22 jump")).is_none());
    }

    #[test]
    fn remote_command_with_tty_is_a_session() {
        let (args, dest) = parse_ssh_args(&argv("-t dev tmux attach")).unwrap();
        assert_eq!(dest, "dev");
        // -t 与远程命令都不带进上传调用
        assert_eq!(args, ["dev"]);
    }

    #[test]
    fn clustered_flags_and_missing_option_value() {
        let (args, _) = parse_ssh_args(&argv("-4Cp 22 dev")).unwrap();
        assert_eq!(args, ["-4", "-C", "-p", "22", "dev"]);
        assert!(parse_ssh_args(&argv("-p")).is_none());
        assert!(parse_ssh_args(&argv("")).is_none());
    }

    #[test]
    fn ps_output_is_parsed_and_non_sessions_skipped() {
        let text = concat!(
            "\x1e123\0b51a3c4d-7526\x0040\0Ubuntu\0dev\0/home/dev\0/tmp/agent/sock\0/usr/bin/ssh\0ssh\0-p\x0022\0dev\0",
            "\x1e124\0\x005\0Ubuntu\0dev\0/home/dev\0\0/usr/bin/ssh\0ssh\0-N\0tun\0",
            "\x1e125\0\x007\0Ubuntu\0dev\0/home/dev\0\0/usr/bin/ssh\0ssh\0box\0"
        );
        let sessions = parse_ps_output(text, 1_000);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].pid, 123);
        assert_eq!(sessions[0].destination, "dev");
        assert_eq!(sessions[0].started, 960);
        assert_eq!(sessions[0].backend, SshBackend::Wsl);
        assert_eq!(sessions[0].wt_session.as_deref(), Some("b51a3c4d-7526"));
        assert_eq!(sessions[1].destination, "box");
        assert_eq!(sessions[1].wt_session, None);
    }

    #[test]
    fn pick_prefers_foreground_tab_then_foreground_app() {
        let sessions = || {
            ["a", "b", "c"]
                .map(|d| {
                    session_from_argv(SshBackend::Windows, 1, "ssh.exe".into(), &argv(d), 0)
                        .unwrap()
                })
                .to_vec()
        };
        let chosen = |focus: &[Focus]| pick(sessions(), focus).map(|(s, _)| s.destination);
        use Focus::*;
        // 前台 tab 里的会话优先，哪怕另一个会话打开得更晚
        assert_eq!(chosen(&[Maybe, Active, Inactive]).as_deref(), Some("b"));
        // 分不出 tab：前台程序里最近打开的
        assert_eq!(chosen(&[Inactive, Maybe, Maybe]).as_deref(), Some("b"));
        // 都不在前台（如在本地 WSL tab 里按热键）：粘本地路径
        assert_eq!(chosen(&[Inactive, Inactive, Inactive]), None);
    }

    #[test]
    fn ssh_options_allow_authentication_without_overriding_routes() {
        let session = session_from_argv(
            SshBackend::Windows,
            1,
            "ssh.exe".into(),
            &argv("-p 2222 -i key -J jump dev"),
            0,
        )
        .unwrap();
        let args = ssh_options(&session, "worker", false);
        assert_eq!(args[0], "-T");
        assert!(args.contains(&"BatchMode=no".to_string()));
        assert!(args.contains(&"ControlMaster=no".to_string()));
        assert_eq!(
            &args[args.len() - 8..],
            ["-p", "2222", "-i", "key", "-J", "jump", "dev", "worker"]
        );
        assert!(ssh_options(&session, "worker", true).contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn only_authentication_errors_enable_dialogs() {
        assert!(authentication_needed(
            "Permission denied (publickey,password)."
        ));
        assert!(!authentication_needed("Connection refused"));
        assert!(!authentication_needed(
            "REMOTE HOST IDENTIFICATION HAS CHANGED! Host key verification failed."
        ));
    }

    #[test]
    fn same_target_ignores_pid() {
        let a = session_from_argv(SshBackend::Windows, 1, "ssh.exe".into(), &argv("h"), 0).unwrap();
        let b = session_from_argv(SshBackend::Windows, 2, "ssh.exe".into(), &argv("h"), 9).unwrap();
        let c = session_from_argv(SshBackend::Wsl, 1, "ssh".into(), &argv("h"), 0).unwrap();
        assert!(a.same_target(&b));
        assert!(!a.same_target(&c));
    }

    #[test]
    fn connection_identity_separates_clients_and_authentication_environments() {
        let a = session_from_argv(SshBackend::Windows, 1, "ssh.exe".into(), &argv("h"), 0).unwrap();
        let mut b = a.clone();
        b.program = "other-ssh.exe".into();
        assert!(!a.same_target(&b));
        b = a.clone();
        b.cwd = Some("C:\\other".into());
        assert!(!a.same_target(&b));
        b = a.clone();
        b.auth_sock = Some("/tmp/other-agent".into());
        assert!(!a.same_target(&b));
        b = a.clone();
        b.wsl_context = Some(("Other".into(), "dev".into()));
        assert!(!a.same_target(&b));
    }

    #[test]
    fn wsl_preserves_agent_and_paths_with_spaces() {
        let text = "\x1e12\0tab\x001\0Ubuntu\0dev\0/home/dev/my work\0/tmp/agent socket\0/usr/bin/ssh\0ssh\0-i\0keys/my key\0box\0";
        let sessions = parse_ps_output(text, 10);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.args, ["-i", "keys/my key", "box"]);
        assert_eq!(session.cwd.as_deref(), Some("/home/dev/my work"));
        assert_eq!(session.auth_sock.as_deref(), Some("/tmp/agent socket"));
        assert_eq!(session.wsl_prefix(), ["-d", "Ubuntu", "-u", "dev"]);
    }
}

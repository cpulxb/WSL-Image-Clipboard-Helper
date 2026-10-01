//! 远程粘贴（issue #11）：自动发现当前打开的 ssh 会话，把本地文件用同一份 ssh 参数
//! 上传到远程主机，返回远程路径。用户不需要任何配置。
//!
//! 发现：枚举 Windows 侧的 `ssh.exe` 进程（读取命令行）和 WSL 默认发行版里的 `ssh`
//! 进程，只保留交互式登录会话（排除 `-N` 隧道、`-T`/VS Code Remote、`-W`/`-O` 等）。
//! 只上传到用户正要粘贴的那个终端 tab 里的会话（见 [`crate::foreground`]）：
//! 同一个 Windows Terminal 里本地 tab 和 ssh tab 并存时，在本地 tab 按热键仍粘本地路径。
//! 托盘菜单可手动固定某个会话。
//!
//! 传输：只用一条 ssh 连接完成"建目录 + 写文件 + 回显绝对路径"：远程执行
//! `mkdir -p DIR && cat > DIR/FILE && printf '%s\n' DIR/FILE`，本地文件作为 ssh 的
//! stdin 流式送过去。不依赖远程装有 scp / sftp-server。
//!
//! 认证：程序没有控制台，无法输入密码，因此新建的 ssh 连接必须能免密登录
//! （公钥认证）。这是所有"独立进程上传"方案共同的前提。

use anyhow::{anyhow, bail, Context, Result};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tracing::{info, warn};

use crate::foreground::{self, Anchor, Focus};

/// 单次上传（含建连）的最长等待
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// 退出清理的最长等待，不能拖住退出流程
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
/// ssh 建连超时（秒），主机不可达时尽快失败
const CONNECT_TIMEOUT_SECS: u32 = 5;
/// CREATE_NO_WINDOW：本程序是无控制台的 GUI 进程，
/// 不加该标志启动 ssh.exe / wsl.exe 会闪出一个黑色控制台窗口
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 远程存放目录（shell 表达式）：按 uid 区分，避免多人共用一台主机时 /tmp 下目录归属冲突
const REMOTE_DIR_EXPR: &str = "/tmp/wsl_clipboard-\"$(id -u)\"";

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
}

impl SshSession {
    pub fn label(&self) -> String {
        format!("{}  ({})", self.destination, self.backend.display_name())
    }

    /// 是否指向同一目标。忽略 pid：同一个 tab 断线重连后仍视为同一目标
    pub fn same_target(&self, other: &SshSession) -> bool {
        self.backend == other.backend && self.args == other.args
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
    Pinned(SshSession),
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
            let anchor_pid = match anchor { Anchor::Process(pid) => Some(*pid), Anchor::AnyWsl => None };
            info!(ssh_pid = session.pid, ?anchor_pid, ?focus,
                "SSH 会话与前台 tab 的匹配结果");
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

        session_from_argv(SshBackend::Windows, pid, program, argv.get(1..)?, started)
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

/// 列出 WSL 里的 ssh 进程：每行 `pid WT_SESSION etimes args...`（无 WT_SESSION 时为 `-`）。
/// 在 tmux / screen 里的 ssh 不报 WT_SESSION：它继承的是 tmux 服务端启动时那个 tab 的值，
/// 不代表用户现在从哪个 tab 看它
const WSL_PS_SCRIPT: &str = r#"ps -o pid=,etimes=,args= -C ssh | while read -r pid rest; do e=$(tr '\0' '\n' < /proc/$pid/environ 2>/dev/null); wt=$(printf '%s\n' "$e" | sed -n 's/^WT_SESSION=//p'); printf '%s\n' "$e" | grep -q -e '^TMUX=' -e '^STY=' && wt=; echo "$pid ${wt:--} $rest"; done"#;

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

/// 解析 [`WSL_PS_SCRIPT`] 的输出。args 只按空白切分：
/// 交互式 ssh 命令行里几乎不会出现带引号的参数
fn parse_ps_output(text: &str, now: u64) -> Vec<SshSession> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid: u32 = it.next()?.parse().ok()?;
            let wt_session = Some(it.next()?).filter(|s| *s != "-").map(str::to_string);
            let elapsed: u64 = it.next()?.parse().ok()?;
            // 第一个 token 是 ssh 程序本身
            let argv: Vec<String> = it.skip(1).map(str::to_string).collect();
            let mut session = session_from_argv(
                SshBackend::Wsl,
                pid,
                PathBuf::from("ssh"),
                &argv,
                now.saturating_sub(elapsed),
            )?;
            session.wt_session = wt_session;
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

struct UploadRecord {
    session: SshSession,
    local: PathBuf,
    remote: String,
    /// 是否为本工具生成的临时截图（退出时只清理这类文件，用户自己复制的文件不动）
    temp_image: bool,
}

pub struct RemoteUploader {
    /// 本会话已成功上传的记录：重复粘贴时跳过上传，退出时据此清理远程临时图
    uploaded: Mutex<Vec<UploadRecord>>,
}

impl RemoteUploader {
    pub fn new() -> Self {
        Self {
            uploaded: Mutex::new(Vec::new()),
        }
    }

    /// 上传 `local` 到远程的 `/tmp/wsl_clipboard-<uid>/<文件名>`，返回远程绝对路径。
    /// 同一目标、同一本地文件只上传一次：剪贴板未变化的重复粘贴直接复用远程路径。
    pub async fn upload(
        &self,
        session: &SshSession,
        local: &Path,
        temp_image: bool,
    ) -> Result<String> {
        let file_name = local
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow!("无效的文件名: {}", local.display()))?;

        if let Some(remote) = self.find_uploaded(session, local) {
            info!("远程已存在，跳过上传: {}", remote);
            return Ok(remote);
        }

        let file = std::fs::File::open(local)
            .with_context(|| format!("打开本地文件失败: {}", local.display()))?;

        info!("上传到 {}: {}", session.label(), local.display());
        let stdout = run_ssh(
            session,
            &upload_command(file_name),
            Stdio::from(file),
            UPLOAD_TIMEOUT,
        )
        .await?;

        // 远程回显的才是展开后的绝对路径；粘贴给 CLI 的必须是绝对路径
        let remote_path = stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("远程没有回显文件路径"))?;

        if let Ok(mut list) = self.uploaded.lock() {
            list.push(UploadRecord {
                session: session.clone(),
                local: local.to_path_buf(),
                remote: remote_path.clone(),
                temp_image,
            });
        }
        Ok(remote_path)
    }

    fn find_uploaded(&self, session: &SshSession, local: &Path) -> Option<String> {
        let list = self.uploaded.lock().ok()?;
        list.iter()
            .find(|r| r.session.same_target(session) && r.local == local)
            .map(|r| r.remote.clone())
    }

    /// 退出时删除本会话上传的临时截图，与本地 `temp/` 的退出清理策略一致
    pub async fn cleanup_on_exit(&self) {
        let grouped: Vec<(SshSession, Vec<String>)> = {
            let Ok(mut list) = self.uploaded.lock() else {
                return;
            };
            let mut grouped: Vec<(SshSession, Vec<String>)> = Vec::new();
            for record in list.drain(..).filter(|r| r.temp_image) {
                match grouped
                    .iter_mut()
                    .find(|(s, _)| s.same_target(&record.session))
                {
                    Some((_, paths)) => paths.push(record.remote),
                    None => grouped.push((record.session, vec![record.remote])),
                }
            }
            grouped
        };

        for (session, paths) in grouped {
            let quoted: Vec<String> = paths.iter().map(|p| sh_quote(p)).collect();
            let cmd = format!("rm -f {}", quoted.join(" "));
            info!(
                "退出清理远程临时图片 ({}): {} 个文件",
                session.label(),
                paths.len()
            );
            if let Err(e) = run_ssh(&session, &cmd, Stdio::null(), CLEANUP_TIMEOUT).await {
                warn!("清理远程临时图片失败 ({}): {}", session.label(), e);
            }
        }
    }
}

/// 执行一次 ssh 远程命令，返回 stdout；失败时把 stderr 最后一行有效信息带进错误
async fn run_ssh(
    session: &SshSession,
    remote_cmd: &str,
    stdin: Stdio,
    timeout: Duration,
) -> Result<String> {
    let (program, args) = ssh_invocation(session, remote_cmd);

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        // 超时后 drop 子进程句柄即杀掉 ssh，避免残留
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .with_context(|| format!("启动 {} 失败", program.display()))?;

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("ssh 超过 {} 秒未完成，已中止", timeout.as_secs()))?
        .context("等待 ssh 结束失败")?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("无错误输出");
    let code = output
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    bail!("ssh 退出码 {}: {}", code, reason)
}

/// 组装 ssh 调用：返回 (程序, 参数)。远程命令作为最后一个参数原样交给 ssh
fn ssh_invocation(session: &SshSession, remote_cmd: &str) -> (PathBuf, Vec<String>) {
    let mut args: Vec<String> = Vec::new();
    let program = match session.backend {
        SshBackend::Windows => session.program.clone(),
        SshBackend::Wsl => {
            // `-e` 直接 exec，不经过 WSL 登录 shell，参数（含引号和 &&）原样到达 ssh
            args.push("-e".into());
            args.push("ssh".into());
            PathBuf::from("wsl.exe")
        }
    };
    // ssh 对同一选项取最先出现的值，因此这几项放在用户参数之前
    args.extend([
        // 没有控制台可交互：不能弹密码 / 指纹确认，直接失败并报错
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", CONNECT_TIMEOUT_SECS),
        // 只保留错误输出，便于把原因显示在托盘气泡里
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
    ]);
    args.extend(session.args.iter().cloned());
    args.push(remote_cmd.to_string());
    (program, args)
}

/// 远程端执行的命令：建目录 → 把 stdin 写成文件 → 回显展开后的绝对路径
fn upload_command(file_name: &str) -> String {
    let file = format!("{}/{}", REMOTE_DIR_EXPR, sh_quote(file_name));
    format!(
        "mkdir -p {} && cat > {} && printf '%s\\n' {}",
        REMOTE_DIR_EXPR, file, file
    )
}

/// POSIX shell 单引号转义
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

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
    fn upload_command_creates_dir_writes_stdin_and_echoes_path() {
        assert_eq!(
            upload_command("clip_1.png"),
            "mkdir -p /tmp/wsl_clipboard-\"$(id -u)\" \
             && cat > /tmp/wsl_clipboard-\"$(id -u)\"/'clip_1.png' \
             && printf '%s\\n' /tmp/wsl_clipboard-\"$(id -u)\"/'clip_1.png'"
        );
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
        let text = "123 b51a3c4d-7526 40 ssh -p 22 dev\n124 - 5 /usr/bin/ssh -N -L 1:2:3 tun\n\
                    125 - 7 ssh box\n garbage\n";
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
                .map(|d| session_from_argv(SshBackend::Windows, 1, "ssh.exe".into(), &argv(d), 0).unwrap())
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
    fn wsl_backend_execs_ssh_through_wsl_exe() {
        let session = session_from_argv(
            SshBackend::Wsl,
            1,
            PathBuf::from("ssh"),
            &argv("-p 22 dev-box"),
            0,
        )
        .unwrap();
        let (program, args) = ssh_invocation(&session, "true");
        assert_eq!(program, PathBuf::from("wsl.exe"));
        assert_eq!(args[..2], ["-e", "ssh"]);
        assert_eq!(args[2..4], ["-o", "BatchMode=yes"]);
        assert_eq!(args[args.len() - 4..], ["-p", "22", "dev-box", "true"]);
    }

    #[test]
    fn windows_backend_reuses_discovered_ssh_exe() {
        let exe = PathBuf::from("C:\\Windows\\System32\\OpenSSH\\ssh.exe");
        let session =
            session_from_argv(SshBackend::Windows, 1, exe.clone(), &argv("root@h"), 0).unwrap();
        let (program, args) = ssh_invocation(&session, "true");
        assert_eq!(program, exe);
        assert_eq!(args[0], "-o");
        assert_eq!(args[args.len() - 2..], ["root@h", "true"]);
        assert_eq!(session.label(), "root@h  (Windows ssh)");
    }

    #[test]
    fn same_target_ignores_pid() {
        let a = session_from_argv(SshBackend::Windows, 1, "ssh.exe".into(), &argv("h"), 0).unwrap();
        let b = session_from_argv(SshBackend::Windows, 2, "ssh.exe".into(), &argv("h"), 9).unwrap();
        let c = session_from_argv(SshBackend::Wsl, 1, "ssh".into(), &argv("h"), 0).unwrap();
        assert!(a.same_target(&b));
        assert!(!a.same_target(&c));
    }
}

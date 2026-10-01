//! OpenSSH askpass entry point. Secrets travel only on the child's stdout pipe.
//! The same exe handles Windows askpass and a tiny, private WSL shell bridge.
use anyhow::{bail, Result};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::Security::Credentials::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use zeroize::Zeroizing;

const NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Must run before single-instance checking and logging (stdout is a secret pipe).
pub fn handle_askpass() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    let (kind, label, event, parent, silent, prompt) =
        if args.get(1).map(String::as_str) == Some("--ssh-askpass") {
            if args.len() != 8 {
                return Some(1);
            }
            (
                args[2].clone(),
                args[3].clone(),
                args[4].clone(),
                args[5].clone(),
                args[6].clone(),
                args[7].clone(),
            )
        } else if std::env::var_os("WSL_CLIPBOARD_ASKPASS").is_some() {
            (
                std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default(),
                std::env::var("WSL_CLIPBOARD_AUTH_LABEL").unwrap_or_default(),
                std::env::var("WSL_CLIPBOARD_AUTH_EVENT").unwrap_or_default(),
                std::env::var("WSL_CLIPBOARD_AUTH_PARENT").unwrap_or_default(),
                std::env::var("WSL_CLIPBOARD_AUTH_SILENT").unwrap_or_default(),
                args.get(1).cloned().unwrap_or_default(),
            )
        } else {
            return None;
        };
    if silent == "1" {
        return Some(1);
    }
    let result = (|| -> Result<()> {
        let parent: u32 = parent.parse()?;
        let event_w = wide(&event);
        let stop =
            unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(event_w.as_ptr())) }?;
        let owner = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent) } {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(stop);
                }
                return Err(e.into());
            }
        };
        // Cancel an orphaned dialog when this attempt ends, times out, or the app exits.
        std::thread::spawn(move || unsafe {
            WaitForMultipleObjects(&[stop, owner], false, 180_000);
            std::process::exit(1);
        });
        let title = wide(&format!("WSL Clipboard Helper — {}", label));
        let message = wide(&prompt);
        if kind == "confirm" || prompt.contains("Are you sure you want to continue connecting") {
            let answer = unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(message.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND,
                )
            };
            if answer != IDYES {
                bail!("cancelled");
            }
            std::io::stdout().write_all(b"yes\n")?;
        } else if kind == "none" {
            unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(message.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
                );
            }
        } else {
            let message = wide(&format!(
                "{}\n\n输入服务器密码、密钥口令或验证码。仅用于本次认证，不保存。",
                prompt
            ));
            let target = wide("WSL Clipboard Helper SSH");
            let mut username = [0u16; 513];
            let user = wide("SSH");
            username[..user.len()].copy_from_slice(&user);
            let mut password = Zeroizing::new([0u16; 513]);
            let info = CREDUI_INFOW {
                cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
                hwndParent: unsafe { GetForegroundWindow() },
                pszMessageText: PCWSTR(message.as_ptr()),
                pszCaptionText: PCWSTR(title.as_ptr()),
                ..Default::default()
            };
            let mut save = BOOL(0);
            let result = unsafe {
                CredUIPromptForCredentialsW(
                    Some(&info),
                    PCWSTR(target.as_ptr()),
                    None,
                    0,
                    &mut username,
                    &mut password[..],
                    Some(&mut save),
                    CREDUI_FLAGS_GENERIC_CREDENTIALS
                        | CREDUI_FLAGS_ALWAYS_SHOW_UI
                        | CREDUI_FLAGS_DO_NOT_PERSIST
                        | CREDUI_FLAGS_KEEP_USERNAME,
                )
            };
            if result != 0 {
                bail!("authentication cancelled");
            }
            let end = password
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(password.len());
            let secret = Zeroizing::new(String::from_utf16(&password[..end])?);
            // Never print errors or prompt text on this channel, even in diagnostics mode.
            let mut out = std::io::stdout().lock();
            out.write_all(secret.as_bytes())?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
        Ok(())
    })();
    Some(if result.is_ok() { 0 } else { 1 })
}

pub struct Askpass {
    event: HANDLE,
    name: String,
    exe: PathBuf,
    label: String,
    silent: bool,
    wsl_dir: Option<String>,
    wsl_prefix: Vec<String>,
}

impl Askpass {
    pub fn new(label: String, silent: bool) -> Result<Self> {
        let name = format!("Local\\WSLClipboardAuth-{:032x}", GUID::new()?.to_u128());
        let w = wide(&name);
        let exe = std::env::current_exe()?;
        let event = unsafe { CreateEventW(None, true, false, PCWSTR(w.as_ptr())) }?;
        Ok(Self {
            event,
            name,
            exe,
            label,
            silent,
            wsl_dir: None,
            wsl_prefix: Vec::new(),
        })
    }

    pub fn windows_env(&self, cmd: &mut Command) {
        cmd.env("SSH_ASKPASS", &self.exe)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "wsl-clipboard:0")
            .env("WSL_CLIPBOARD_ASKPASS", "1")
            .env("WSL_CLIPBOARD_AUTH_LABEL", &self.label)
            .env("WSL_CLIPBOARD_AUTH_EVENT", &self.name)
            .env("WSL_CLIPBOARD_AUTH_PARENT", std::process::id().to_string())
            .env(
                "WSL_CLIPBOARD_AUTH_SILENT",
                if self.silent { "1" } else { "0" },
            );
    }

    pub async fn wsl_script(&mut self, prefix: &[String]) -> Result<String> {
        self.wsl_prefix = prefix.to_vec();
        let script = r#"set -eu
umask 077
exe=$(wslpath -u "$1")
[ -x "$exe" ]
dir=$(mktemp -d /tmp/wsl-clipboard-askpass.XXXXXXXXXX)
cat > "$dir/askpass"
chmod 700 "$dir/askpass"
printf '%s\n' "$dir"
"#;
        // The executable path is converted in the target WSL environment. All other
        // bridge arguments are quoted literals; the SSH prompt remains one argument.
        let mut convert = Command::new("wsl.exe");
        convert
            .args(prefix)
            .args(["-e", "wslpath", "-u"])
            .arg(&self.exe)
            .creation_flags(NO_WINDOW);
        let out = tokio::time::timeout(Duration::from_secs(10), convert.output()).await??;
        if !out.status.success() {
            bail!("WSL 无法访问认证窗口程序");
        }
        let exe = String::from_utf8(out.stdout)?.trim().to_string();
        let shim = format!(
            "#!/bin/sh\nexec {} --ssh-askpass \"${{SSH_ASKPASS_PROMPT:-}}\" {} {} {} {} \"$@\"\n",
            quote(&exe),
            quote(&self.label),
            quote(&self.name),
            std::process::id(),
            if self.silent { "1" } else { "0" }
        );
        let mut cmd = Command::new("wsl.exe");
        cmd.args(prefix)
            .args(["-e", "sh", "-c", script, "sh"])
            .arg(&self.exe)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .creation_flags(NO_WINDOW);
        let mut child = cmd.spawn()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(shim.as_bytes())
            .await?;
        let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await??;
        if !out.status.success() {
            bail!(
                "创建 WSL 认证桥失败：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let dir = String::from_utf8(out.stdout)?.trim().to_string();
        if !dir.starts_with("/tmp/wsl-clipboard-askpass.") || dir.contains(['\n', '\r', ' ']) {
            bail!("WSL 认证桥路径异常");
        }
        let path = format!("{dir}/askpass");
        self.wsl_dir = Some(dir);
        Ok(path)
    }
}

impl Drop for Askpass {
    fn drop(&mut self) {
        unsafe {
            let _ = SetEvent(self.event);
            let _ = CloseHandle(self.event);
        }
        if let Some(dir) = &self.wsl_dir {
            let _ = std::process::Command::new("wsl.exe")
                .args(&self.wsl_prefix)
                .args([
                    "-e",
                    "sh",
                    "-c",
                    "rm -f -- \"$1/askpass\"; rmdir -- \"$1\"",
                    "sh",
                    dir,
                ])
                .creation_flags(NO_WINDOW)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

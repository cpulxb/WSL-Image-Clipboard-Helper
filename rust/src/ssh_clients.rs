//! Client-specific discovery adapters. Uploading stays in the existing OpenSSH transport.
//! A GUI client's saved session is not an OpenSSH command line. Never guess credentials
//! or routing when its settings cannot be represented by this adapter.
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    OpenSsh,
    MobaXterm,
}

impl Client {
    pub fn from_executable(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("ssh.exe") {
            Some(Self::OpenSsh)
        } else if name.eq_ignore_ascii_case("motty.exe") {
            Some(Self::MobaXterm)
        } else {
            None
        }
    }
}

pub struct Options {
    pub args: Vec<String>,
    pub label: String,
}

pub fn windows_openssh() -> PathBuf {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buffer = [0u16; 32768];
    let n = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if n == 0 || n >= buffer.len() {
        return PathBuf::new();
    }
    PathBuf::from(String::from_utf16_lossy(&buffer[..n]))
        .join("OpenSSH")
        .join("ssh.exe")
}

fn loaded_session(argv: &[String]) -> Result<&str> {
    // MobaXterm creates a temporary TERM session. Additional CLI overrides must be
    // explicitly supported before we can safely translate them.
    if argv.len() != 2
        || argv[0] != "-load"
        || argv[1].is_empty()
        || argv[1]
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        bail!("无法读取此 MoTTY 启动方式，仅支持 MobaXterm 保存的 SSH 会话（-load）。");
    }
    Ok(&argv[1])
}

pub fn moba_options(argv: &[String]) -> Result<Options> {
    let name = loaded_session(argv)?;
    let key = RegistrySession::new(name);
    let settings = MobaSettings {
        protocol: key
            .text("Protocol")
            .context("会话配置不可读，请重新连接该标签页")?,
        host: key.text("HostName").context("缺少服务器地址")?,
        user: key.text("UserName").context("缺少用户名")?,
        port: key.number("PortNumber").context("缺少端口")?,
        identity: key.text("PublicKeyFile").unwrap_or_default(),
        proxy: key.number("ProxyMethod").context("无法确认代理设置")?,
        tunnel: key.text("TunneledHostname").unwrap_or_default(),
        remote_command: key.text("RemoteCommand").unwrap_or_default(),
        shift_insert: key.number("PasteUsingShiftInsert").unwrap_or(0) != 0,
    };
    let options = settings.options()?;
    if !windows_openssh().is_file() {
        bail!("未找到 Windows OpenSSH 客户端，请先安装 Windows 的 OpenSSH 客户端可选功能。");
    }
    let key_path = normalize_moba_path(&settings.identity);
    // Inspect only the header, never log, copy, alter ACLs, or persist private-key bytes.
    let mut header = [0u8; 80];
    let count = std::fs::File::open(&key_path)
        .context("无法读取会话指定的密钥文件")?
        .read(&mut header)?;
    let header = String::from_utf8_lossy(&header[..count]);
    if !supported_key_header(&header) {
        bail!("此密钥格式不能交给 OpenSSH 使用（例如 PuTTY .ppk）。请使用已有的 PEM/OpenSSH 密钥，或在 Windows Terminal 中建立 OpenSSH 会话。");
    }
    Ok(options)
}

fn supported_key_header(header: &str) -> bool {
    [
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN DSA PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    ]
    .iter()
    .any(|prefix| header.starts_with(prefix))
}

struct MobaSettings {
    protocol: String,
    host: String,
    user: String,
    port: u32,
    identity: String,
    proxy: u32,
    tunnel: String,
    remote_command: String,
    shift_insert: bool,
}

impl MobaSettings {
    fn options(&self) -> Result<Options> {
        if self.protocol != "ssh" {
            bail!("当前会话不是 SSH。");
        }
        if self.proxy != 0 || !self.tunnel.is_empty() {
            bail!("此会话使用代理或 SSH 跳板，当前 MobaXterm 适配尚不支持；请用 OpenSSH 的 -J 或 SSH config 建立会话。");
        }
        if !self.remote_command.is_empty() {
            bail!("此会话包含自定义远程命令，暂不支持自动转换。");
        }
        if !self.shift_insert {
            bail!("请先在 MobaXterm 设置中启用 Shift+Insert 粘贴。");
        }
        if self.host.is_empty()
            || self.host.starts_with('-')
            || self
                .host
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '@' | '/' | '\\' | '\0'))
            || self.user.is_empty()
            || self.user.starts_with('-')
            || self.user.chars().any(|c| c.is_control())
            || !(1..=65535).contains(&self.port)
        {
            bail!("会话中的主机、用户名或端口无效，已停止上传。");
        }
        if self.identity.is_empty() {
            bail!("此会话未指定密钥文件。当前适配支持 PEM/OpenSSH 密钥，不能复用 MobaXterm 保存的密码或 MobAgent；可改用 Windows/WSL OpenSSH 会话。");
        }
        let identity = normalize_moba_path(&self.identity);
        if !PathBuf::from(&identity).is_absolute() || identity.contains('\0') {
            bail!("会话的密钥路径不是有效的 Windows 绝对路径。");
        }
        Ok(Options {
            // Do not let unrelated Windows Host * / ProxyCommand settings redirect
            // the GUI session. Known-host checking stays enabled with OpenSSH defaults.
            args: vec![
                "-F".into(),
                "NUL".into(),
                "-o".into(),
                "IdentitiesOnly=yes".into(),
                "-i".into(),
                identity,
                "-p".into(),
                self.port.to_string(),
                "-l".into(),
                self.user.clone(),
                self.host.clone(),
            ],
            label: format!("{}@{}:{}", self.user, self.host, self.port),
        })
    }
}

fn normalize_moba_path(value: &str) -> String {
    // MobaXterm doubles each backslash in registry strings, including UNC paths.
    value.replace("\\\\", "\\")
}

struct RegistrySession {
    path: Vec<u16>,
}
impl RegistrySession {
    fn new(name: &str) -> Self {
        let path = format!("Software\\MobaXterm\\MoTTY\\Sessions\\{name}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        Self { path }
    }
    fn text(&self, name: &str) -> Option<String> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut buffer = [0u16; 32768];
        let mut bytes = (buffer.len() * 2) as u32;
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(self.path.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
            .ok()?;
        }
        let n = (bytes as usize / 2).min(buffer.len());
        Some(
            String::from_utf16_lossy(&buffer[..n])
                .trim_end_matches('\0')
                .to_string(),
        )
    }
    fn number(&self, name: &str) -> Option<u32> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut value = 0u32;
        let mut bytes = 4u32;
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(self.path.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut value as *mut u32).cast()),
                Some(&mut bytes),
            )
            .ok()?;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> MobaSettings {
        MobaSettings {
            protocol: "ssh".into(),
            host: "example.test".into(),
            user: "ubuntu".into(),
            port: 2222,
            identity: r"C:\\Users\\Test User\\server.pem".into(),
            proxy: 0,
            tunnel: String::new(),
            remote_command: String::new(),
            shift_insert: true,
        }
    }
    #[test]
    fn preserves_key_spaces_port_and_user_without_a_shell() {
        let result = settings().options().unwrap();
        assert_eq!(
            result.args,
            [
                "-F",
                "NUL",
                "-o",
                "IdentitiesOnly=yes",
                "-i",
                r"C:\Users\Test User\server.pem",
                "-p",
                "2222",
                "-l",
                "ubuntu",
                "example.test"
            ]
        );
        assert_eq!(
            normalize_moba_path(r"\\\\server\\share\\key.pem"),
            r"\\server\share\key.pem"
        );
    }
    #[test]
    fn rejects_routing_and_authentication_that_cannot_be_preserved() {
        let mut s = settings();
        s.proxy = 5;
        assert!(s.options().is_err());
        s = settings();
        s.tunnel = "target".into();
        assert!(s.options().is_err());
        s = settings();
        s.identity.clear();
        assert!(s.options().is_err());
        s = settings();
        s.remote_command = "sudo su -".into();
        assert!(s.options().is_err());
        s = settings();
        s.shift_insert = false;
        assert!(s.options().is_err());
        s = settings();
        s.host = "-oProxyCommand=bad".into();
        assert!(s.options().is_err());
        s = settings();
        s.port = 65536;
        assert!(s.options().is_err());
        assert!(!supported_key_header("PuTTY-User-Key-File-3: ssh-rsa"));
        assert!(supported_key_header("-----BEGIN RSA PRIVATE KEY-----\n"));
    }
    #[test]
    fn refuses_unknown_clients_and_unhandled_cli_overrides() {
        assert_eq!(
            Client::from_executable("MoTTY.exe"),
            Some(Client::MobaXterm)
        );
        assert_eq!(Client::from_executable("PuTTY.exe"), None);
        assert_eq!(Client::from_executable("ssh.exe"), Some(Client::OpenSsh));
        assert!(loaded_session(&["-load".into(), "TERM123".into()]).is_ok());
        assert!(
            loaded_session(&["-load".into(), "TERM123".into(), "-P".into(), "22".into()]).is_err()
        );
        assert!(loaded_session(&["-load".into(), "..\\other".into()]).is_err());
    }
}

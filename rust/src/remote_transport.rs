//! Framed binary transfers over one long-lived OpenSSH child (also used by WSL).
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

pub const WORKER: &str = include_str!("remote_worker.sh");
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);

struct Uploaded {
    local: PathBuf,
    remote: String,
}

pub struct Connection {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    token: String,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
    uploaded: Vec<Uploaded>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // kill_on_drop closes the SSH channel; the remote EXIT trap cleans screenshots.
        self.stderr_task.abort();
    }
}

impl Connection {
    pub async fn start(mut cmd: Command, token: String, timeout: Duration) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("启动 SSH 上传连接失败")?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut errors = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = stderr.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buf = [0; 2048];
            while let Ok(n) = errors.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut tail = sink.lock().unwrap();
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 8192 {
                    let excess = tail.len() - 8192;
                    tail.drain(..excess);
                }
            }
        });
        let mut conn = Self {
            child,
            input: Some(input),
            output,
            token,
            stderr,
            stderr_task,
            uploaded: Vec::new(),
        };
        match tokio::time::timeout(timeout, conn.expect("READY")).await {
            Ok(Ok(_)) => Ok(conn),
            Ok(Err(e)) => {
                // Let stderr drain before classifying authentication errors.
                let _ = tokio::time::timeout(Duration::from_secs(1), conn.child.wait()).await;
                let _ = tokio::time::timeout(Duration::from_secs(1), &mut conn.stderr_task).await;
                Err(anyhow!("{}: {}", e, conn.error_text()))
            }
            Err(_) => bail!("SSH 认证等待超时，已取消；请再次按热键重试"),
        }
    }

    fn error_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap())
            .trim()
            .to_string()
    }

    async fn expect(&mut self, expected: &str) -> Result<String> {
        let prefix = format!("{} ", self.token);
        let mut skipped = 0;
        loop {
            // Bound banners / broken peers instead of an unbounded read_line allocation.
            let mut line = Vec::new();
            let n = (&mut self.output)
                .take(65536)
                .read_until(b'\n', &mut line)
                .await?;
            if n == 0 {
                bail!("SSH 连接已关闭");
            }
            if n == 65536 {
                bail!("SSH 返回的数据过长");
            }
            let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
            if let Some(reply) = line.strip_prefix(&prefix) {
                if reply == expected {
                    return Ok(String::new());
                }
                if let Some(value) = reply.strip_prefix(&format!("{} ", expected)) {
                    return Ok(value.to_string());
                }
                bail!("SSH 上传协议响应异常");
            }
            skipped += n;
            if skipped > 65536 {
                bail!("SSH 登录输出过长");
            }
        }
    }

    pub async fn healthy(&mut self) -> bool {
        if self.input.is_none() || !matches!(self.child.try_wait(), Ok(None)) {
            return false;
        }
        matches!(
            tokio::time::timeout(Duration::from_secs(5), async {
                self.input.as_mut().unwrap().write_all(b"PING\n").await?;
                self.input.as_mut().unwrap().flush().await?;
                self.expect("PONG").await
            })
            .await,
            Ok(Ok(_))
        )
    }

    pub async fn upload(&mut self, local: &Path, temporary: bool) -> Result<String> {
        if let Some(upload) = self.uploaded.iter().find(|f| f.local == local) {
            return Ok(upload.remote.clone());
        }
        let result = tokio::time::timeout(TRANSFER_TIMEOUT, self.transfer(local, temporary)).await;
        match result {
            Ok(Ok(path)) => {
                self.uploaded.push(Uploaded {
                    local: local.into(),
                    remote: path.clone(),
                });
                Ok(path)
            }
            Ok(Err(e)) => Err(e.context(format!("SSH 上传失败：{}", self.error_text()))),
            Err(_) => bail!("SSH 上传超过 60 秒，连接已中止"),
        }
    }

    async fn transfer(&mut self, local: &Path, temporary: bool) -> Result<String> {
        let name = local
            .file_name()
            .and_then(|s| s.to_str())
            .context("无效的文件名")?;
        let encoded = encode_name(name)?;
        let mut file = tokio::fs::File::open(local)
            .await
            .context("打开上传文件失败")?;
        let size = file.metadata().await?.len();
        let kind = if temporary { 't' } else { 'f' };
        self.input
            .as_mut()
            .unwrap()
            .write_all(format!("PUT {size} {kind} {encoded}\n").as_bytes())
            .await?;
        self.input.as_mut().unwrap().flush().await?;
        self.expect("SEND").await?;
        let sent =
            tokio::io::copy(&mut (&mut file).take(size), self.input.as_mut().unwrap()).await?;
        if sent != size {
            bail!("上传期间本地文件大小发生变化");
        }
        self.input.as_mut().unwrap().flush().await?;
        let path = self.expect("OK").await?;
        if !path.starts_with("/tmp/wsl_clipboard-") || path.contains(['\r', '\n']) {
            bail!("SSH 返回了无效的远程路径");
        }
        Ok(path)
    }

    pub async fn close(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            if let Some(mut input) = self.input.take() {
                input.write_all(b"QUIT\n").await?;
                // ChildStdin::shutdown only flushes on Windows. Drop the actual pipe
                // so interrupted frames see EOF and the remote EXIT trap can run.
                drop(input);
            }
            self.child.wait().await
        })
        .await;
    }
}

fn encode_name(name: &str) -> Result<String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\r', '\n', '\0'])
    {
        bail!("不支持的上传文件名");
    }
    Ok(name
        .as_bytes()
        .iter()
        .map(|b| format!("\\0{b:03o}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_inject_protocol_or_shell_commands() {
        assert_eq!(
            encode_name("a b'$").unwrap(),
            "\\0141\\0040\\0142\\0047\\0044"
        );
        for name in ["", "..", "a/b", "a\nb", "a\rb", "a\0b"] {
            assert!(encode_name(name).is_err());
        }
    }

    // Run on Windows through the same WSL bridge as the product; on Linux directly.
    fn worker_command(token: &str) -> Command {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("wsl.exe");
            c.args(["-e", "sh"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = Command::new("sh");
        cmd.args(["-c", WORKER, "sh", token]);
        cmd
    }

    async fn remote_command(args: &[&str]) -> std::process::Output {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("wsl.exe");
            c.arg("-e").args(args);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new(args[0]);
            c.args(&args[1..]);
            c
        };
        cmd.output().await.unwrap()
    }

    #[tokio::test]
    async fn binary_transfers_cache_and_exit_cleanup() {
        let token = format!("test{}", std::process::id());
        let mut c = Connection::start(worker_command(&token), token, Duration::from_secs(10))
            .await
            .unwrap();
        let local_dir =
            std::env::temp_dir().join(format!("clipboard-transfer-{}", std::process::id()));
        std::fs::create_dir_all(&local_dir).unwrap();
        let local = local_dir.join("测试 '$(echo bad)'.png");
        let data: Vec<u8> = (0..1024 * 1024).map(|n| (n % 256) as u8).collect();
        std::fs::write(&local, &data).unwrap();
        let path = c.upload(&local, true).await.unwrap();
        assert!(c.healthy().await);
        assert_eq!(c.upload(&local, true).await.unwrap(), path);
        #[cfg(windows)]
        let output = Command::new("wsl.exe")
            .args(["-e", "cat", &path])
            .output()
            .await
            .unwrap();
        #[cfg(not(windows))]
        let output = Command::new("cat").arg(&path).output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, data);
        let empty = local_dir.join("empty.png");
        std::fs::write(&empty, []).unwrap();
        c.upload(&empty, true).await.unwrap();
        let ordinary = local_dir.join("keep.txt");
        std::fs::write(&ordinary, b"keep this copied file").unwrap();
        let kept = c.upload(&ordinary, false).await.unwrap();
        c.close().await;
        assert!(!c.healthy().await);
        #[cfg(windows)]
        let status = Command::new("wsl.exe")
            .args(["-e", "test", "!", "-e", &path])
            .status()
            .await
            .unwrap();
        #[cfg(not(windows))]
        let status = Command::new("test")
            .args(["!", "-e", &path])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(
            remote_command(&["cat", &kept]).await.stdout,
            b"keep this copied file"
        );
        assert!(remote_command(&["rm", "--", &kept]).await.status.success());
        let parent = kept.rsplit_once('/').unwrap().0;
        assert!(remote_command(&["rmdir", "--", parent])
            .await
            .status
            .success());
        std::fs::remove_dir_all(local_dir).unwrap();
    }

    #[tokio::test]
    async fn interrupted_frame_exits_and_cleans_partial_data() {
        let token = format!("partial{}", std::process::id());
        let mut c = Connection::start(worker_command(&token), token, Duration::from_secs(10))
            .await
            .unwrap();
        let local = std::env::temp_dir().join(format!("wch-partial-{}.png", std::process::id()));
        std::fs::write(&local, b"complete").unwrap();
        let first = c.upload(&local, true).await.unwrap();
        c.input
            .as_mut()
            .unwrap()
            .write_all(format!("PUT 100 t {}\n", encode_name("incomplete.png").unwrap()).as_bytes())
            .await
            .unwrap();
        c.input.as_mut().unwrap().flush().await.unwrap();
        c.expect("SEND").await.unwrap();
        c.input.as_mut().unwrap().write_all(b"short").await.unwrap();
        drop(c.input.take());
        let status = tokio::time::timeout(Duration::from_secs(5), c.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        let parent = first.rsplit_once('/').unwrap().0;
        assert!(remote_command(&["test", "!", "-e", parent])
            .await
            .status
            .success());
        std::fs::remove_file(local).unwrap();
    }
}

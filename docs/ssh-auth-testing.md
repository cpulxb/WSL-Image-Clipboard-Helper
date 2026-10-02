# SSH 认证与连接复用验证

对应 v4.2 的 SSH 认证与连接复用功能。功能实现提交为 `26432b1`，之前的远程粘贴基线为 `361b0a8`。

2026-10-01 本机运行结果：27 项单元测试通过、1 项已有隔离剪贴板测试按默认设置忽略；17 个 SSH 集成检查通过。

另外，对 release 可执行文件做了原生凭据窗口冒烟检查：窗口能保持等待，父进程取消后退出，未输出凭据。该检查没有输入任何密码。

用户随后在日常环境试用并反馈未发现问题；该反馈不等同于以下全部场景均已人工覆盖。

## MobaXterm 适配（2026-10-02，v4.2.1）

- Windows 单元测试：31 项通过，1 项已有隔离剪贴板测试默认忽略。覆盖主机、用户名、端口、含空格的密钥路径、MobaXterm 路径转义、终端粘贴扫描码及修饰键顺序，以及拒绝未支持的代理、密钥格式和命令行覆盖。
- 本机回环 SSH 集成检查：19 项通过，含既有的 17 项 Windows/WSL 检查，以及读取临时 MobaXterm 注册表会话后实际上传/缓存/连接复用/清理、拒绝变更的服务器主机密钥两项。仅使用一次性测试密钥、独立 known_hosts 和 `127.0.0.1`。
- 只读检查确认实际 MoTTY 进程、保存会话参数和可见子窗口可被识别，不向终端发送按键，不建立上传连接。
- MobaXterm + 远程 Linux + PEM 密钥登录场景的人工验收通过：`Alt+V` 上传后，远端 Claude Code 显示图片附件。多标签页/多面板切换、首次主机指纹对话框的完整交互和其他 MobaXterm 版本仍未完成专项人工验收。
- 正式实现使用系统 OpenSSH，不调用 MobaXterm 内部传输程序，不修改密钥权限。

终端粘贴使用独立 Insert 的扩展扫描码 `E0 52`，分开发送 Shift 按下、Insert、Shift 松开。自动测试通过 Windows 的扫描码映射接口验证按键标识和修饰键顺序；人工验收同时覆盖实际上传与终端图片识别。个人服务器信息、密钥路径、截图及运行日志不随仓库提交。

本次提交的 exe 使用相同功能代码重新构建，通过 Rust 的 `--remap-path-prefix` 将本机绝对编译路径替换为中性目录；不包含测试凭据或诊断测试入口。隐私检查范围为本次提交的文本、二进制文件和提交身份，不涉及改写已有 Git 历史。

复测 MobaXterm 集成：在 `HKCU\Software\MobaXterm\MoTTY\Sessions` 下创建一个**全新、一次性**测试项（不要修改真实会话），填写 `Protocol=ssh`、`HostName=127.0.0.1`、`UserName=fixture`、测试服务实际 `PortNumber`、`PublicKeyFile` 为测试目录中的 `plain`、`ProxyMethod=0`、`PasteUsingShiftInsert=1`，空 `TunneledHostname` 和 `RemoteCommand`。将该项名称设为 `WCH_MOBA_TEST_SESSION`，同时设置下文的 `WCH_SSH_TEST_DIR`，运行集成测试。测试仅为该连接指定独立的 known_hosts；结束后删除测试项。

测试程序的 `--inspect-moba` 参数只读取当前 MoTTY 会话与窗口归属，不上传或粘贴；该入口不包含在正式程序中。测试服务初始 ControlMaster 的空闲寿命为 10 分钟，准备过程超过它时应先重建测试 master，再验证 `Wsl reused` 用例。

## 已验证的内容

- Windows OpenSSH 9.5 与默认 WSL 的 OpenSSH 都运行真实客户端，连接本机回环地址上的临时 Paramiko SSH 服务。
- 两种客户端均覆盖密码、无口令私钥、带口令私钥、取消认证、错误密码、主机密钥变更、连接失效后重新认证。
- 同一目标新 tab（PID 变化）、重复图片缓存、多个文件连续上传均复用同一条上传连接；退出不再次请求认证。
- WSL 额外覆盖原会话 agent socket，以及已有的、通过密码认证的 ControlMaster 连接；两项均无需额外认证。
- 协议测试覆盖 1 MiB 的全部字节值、中文/空格/引号/命令替换字符文件名、空文件、PING、传输中断后的清理，以及退出时保留普通文件。
- Windows 单元测试覆盖参数解析、认证环境隔离、Windows 工作目录读取，以及既有的剪贴板、前台 tab 判断逻辑。

认证集成测试的 askpass 回答来自**仅在测试二进制中的固定测试数据**，验证的是 OpenSSH 回调、WSL 到 Windows 的桥接、认证重试和连接管理。正式可执行文件没有测试回答入口，也不把密码写入磁盘。

## 验证边界

- 上述自动测试没有模拟人在凭据窗口里输入；MobaXterm + 远程 Linux 的终端图片显示已通过人工验收，其余环境的窗口焦点恢复和完整认证交互仍需分别验收。
- 外部服务器人工验收目前覆盖 PEM 密钥连接；企业 MFA/硬件密钥、Git 自带 SSH 或多层跳板环境尚未验收。OpenSSH 会话仍由原客户端负责认证与路由，参数保持透传；MobaXterm 适配的范围见上文。
- WSL 会话发现仍只扫描默认发行版的默认用户。密码窗口需要启用 WSL Windows interop；已授权密钥、agent 或已有复用连接的静默路径不需要认证桥。

## 重跑单元测试

Windows 上安装 Rust 后，在 `rust/` 执行：

```powershell
cargo test --bin wsl_clipboard -- --test-threads=1
```

二进制传输测试会调用默认 WSL 的 `sh`。已有的真实剪贴板测试默认忽略，因为它需要单独的隔离环境。

从 WSL 交叉编译时需要 Windows SDK/MSVC 库或 cargo-xwin；编译出的测试 exe 可通过 WSL interop 运行。

## 重跑本机 SSH 集成测试

在 WSL 建立独立的测试 Python 环境（Paramiko 仅是测试依赖）：

```bash
python3 -m venv /tmp/wch-ssh-tests
/tmp/wch-ssh-tests/bin/pip install paramiko
/tmp/wch-ssh-tests/bin/python rust/tests/ssh_fixture.py \
  /mnt/c/Users/<Windows用户名>/AppData/Local/Temp/wch-ssh-fixture
```

目录必须尚不存在。脚本仅监听 `127.0.0.1` 的随机端口，创建临时服务端密钥、客户端密钥、known_hosts、独立配置、agent 和 ControlMaster。为让 Windows OpenSSH 接受测试密钥，它仅收紧**新建测试密钥文件**的 ACL；不修改用户 `.ssh`、真实账号、系统服务或防火墙。

在 Windows PowerShell 的 `rust/` 目录中执行（服务需保持运行）：

```powershell
$env:WCH_SSH_TEST_DIR = "$env:TEMP\wch-ssh-fixture"
cargo test --test ssh_integration
```

未设置 `WCH_SSH_TEST_DIR` 时，该显式集成测试会报告跳过。设置后运行 Windows 和 WSL 两组测试；每个用例断言 askpass 次数。成功结果包含 `PASS Windows ...`、`PASS Wsl ...`。

结束时在启动测试服务的终端按 `Ctrl+C`，脚本会结束测试 master/agent 并清理 Linux 临时密钥。Windows 测试目录保留结果日志，可在检查后删除。

可选的原生窗口打开/取消检查（会短暂显示标明本机测试的窗口，无需输入）：

```powershell
$env:WCH_SSH_NATIVE_SMOKE = "1"
$env:WCH_SSH_APP_EXE = (Resolve-Path "target\x86_64-pc-windows-msvc\release\wsl_clipboard.exe").Path
cargo test --test ssh_integration
Remove-Item Env:WCH_SSH_NATIVE_SMOKE, Env:WCH_SSH_APP_EXE
```

## 日常终端人工验收

1. 分别从 PowerShell 与默认 WSL 正常 SSH 登录，启动远程 CLI。
2. 复制截图并按 `Alt+V`，必要时输入服务器密码或私钥口令；确认图片可用。
3. 换一张截图再次粘贴，确认没有重复认证；同环境、同参数新 tab 也应复用。
4. 取消一次认证，确认不误贴；再次按热键应能重试。
5. 认证窗口打开时从托盘退出，确认窗口随之关闭，没有新弹窗。
6. 上传期间切换到别的应用，确认不会粘贴到该应用；返回原终端重试。

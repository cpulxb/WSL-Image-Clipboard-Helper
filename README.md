# WSL Image Clipboard Helper

Language: [中文说明](#中文说明) | [English Guide](#english-guide)

---

## 中文说明

### 概述

#### 背景
当前许多智能编程 CLI Agent（如 Codex 、Amazon Q Developer CLI、OpenCode、Claude Code 等）主要针对 Linux 和 macOS 系统优化，Windows 用户想要体验这些工具，通常需要通过 WSL2（Windows Subsystem for Linux 2）来运行。然而，WSL2 在某些功能上的支持并不完善，**图片粘贴**就是其中一个典型痛点：

- **问题**：WSL2 终端无法直接访问 Windows 剪贴板中的图片数据
- **影响**：用户无法像在原生 Linux/macOS 中那样，直接将截图粘贴给 AI 工具进行分析
- **现状**：一些 AI CLI 工具（如 Amazon Q Developer CLI）通过"保存图片到文件 → 传递文件路径"的方式来变相实现图片输入

#### 解决方案
本工具正是为了弥补这一缺陷而设计：通过 `Alt+V` 快捷键，自动将 Windows 剪贴板中的图片保存到本地，并将对应的 WSL 路径（`/mnt/c/...`）粘贴到当前窗口，让 AI 工具能够无缝读取图片。

**v2.0 版本重点优化**：在保留 `Alt+V` 快捷键工作流的基础上，引入输入法保护、异步保存、自动清理等能力。与旧版需要等待 SHA256 去重与图片落盘相比，现在路径不到 1 秒即可粘贴完成，且不会再出现字符逐个跳出的视觉延迟。

### 核心特性
- **即时路径输出**：`Alt+V` 触发后立即粘贴 `/mnt/...` 路径，无需等待图片写入完成，整体响应时间从约 3 秒缩短到 1 秒以内。
- **输入法智能保护**：粘贴前自动切换至英文输入法，完成后恢复原状态，避免中文输入法导致路径错乱。
- **后台异步保存**：借助 PowerShell 脚本在后台保存图片，确保操作无感延迟，并对错误静默处理。
- **自动清理机制**：定时清理超过 2 小时的临时图片，退出时自动回收缓存与子进程。
- **托盘管理增强**：托盘菜单支持一键打开缓存目录、退出程序，便于日常维护。

### 必备环境
- Windows 10/11，已启用 WSL2
- PowerShell 5.1 及以上，允许执行本地脚本（建议运行 `Set-ExecutionPolicy RemoteSigned -Scope CurrentUser`）
- AutoHotkey v2（已编译为 `wsl_clipboard.exe`；仅在需要重新编译或调试脚本时安装）

### 使用方式
1. 克隆仓库并进入目录：
   ```bash
   git clone https://github.com/cpulxb/WSL-Image-Clipboard-Helper.git
   cd WSL-Image-Clipboard-Helper
   ```
2. 保证 `scripts` 目录下的 `wsl_clipboard.exe` 与相关 `.ps1` 脚本位于同一文件夹。
3. 双击 `scripts/wsl_clipboard.exe`，程序会最小化至系统托盘。
4. 在任意文本输入框按下 `Alt+V`：
   - 剪贴板图片保存至 `temp/` 目录（后台进行）
   - `/mnt/...` 路径立即粘贴至当前窗口
5. 退出时，从托盘图标右键菜单选择 `Exit`，程序会调用 `exit-all.ps1` 清理缓存与子进程。

### 常见注意事项
- `Alt+V` 为全局快捷键，如与其他软件冲突，可编辑 `scripts/wsl_clipboard.ahk` 并重新编译。
- 若托盘图标未显示，请检查任务栏的隐藏图标区域。
- 所有 PowerShell 脚本推荐使用 UTF-8 with BOM 保存，以避免中文内容导致解析失败。
- 可随时运行 `scripts/exit-all.ps1` 手动清理缓存与相关进程。

### 重新编译（可选）

如需自定义热键、修改临时目录路径或分发新的 `.exe`，需先安装 AutoHotkey v2，然后使用自带的 Ahk2Exe 编译器：

1. **安装 AutoHotkey v2**
   - 下载并安装 [AutoHotkey v2 官方版](https://www.autohotkey.com/download/ahk-v2.exe)

2. **修改脚本（可选）**
   - **修改热键**：编辑 `scripts/wsl_clipboard.ahk` 第 18 行，将 `!v::` 改为其他组合键
     - `!v` = Alt+V
     - `^!v` = Ctrl+Alt+V
     - `^+v` = Ctrl+Shift+V
   - **修改临时目录**：编辑第 5 行 `gTempDir` 变量的路径
   - **修改清理间隔**：编辑第 125 行的时间参数（默认 2 小时 = 7200000 毫秒）

3. **编译为可执行文件**
   - 打开 `C:\Program Files\AutoHotkey\Compiler\Ahk2Exe.exe`
   - **Source (script file)**：选择 `scripts\wsl_clipboard.ahk`
   - **Destination (.exe file)**：指定输出路径（如 `scripts\wsl_clipboard.exe`）
   - **Base File (.bin, .exe)**：选择合适的 Base（推荐 `AutoHotkey64.exe`）
   - 点击 `Convert` 开始编译

4. **测试新版本**
   - 先从托盘退出旧版本
   - 双击新编译的 `wsl_clipboard.exe` 测试

### 附加文档
- [技术架构与流程说明](docs/architecture_by_codex.md)

### 版本历史

#### v2.0 (当前版本)
- ✨ **路径优先异步保存**：先粘贴路径，后台保存图片，响应时间从 ~3 秒降至 <1 秒
- 🔤 **输入法智能保护**：自动切换英文输入法，避免中文输入法干扰路径
- 🧹 **自动清理机制**：每 2 小时清理超过 2 小时的临时图片
- 🚀 **代码精简**：PowerShell 脚本从 86 行减少到 28 行（-67%）
- 🐛 **编码修复**：exit-all.ps1 改用 UTF-8 with BOM，支持 emoji 和中文字符
- ❌ **移除缓存文件**：删除 last_output.txt、last_seq.txt、last_hash.txt

#### v1.0
- 基础剪贴板图片同步功能
- SHA256 去重机制
- 缓存文件管理

---

## English Guide

### Overview

#### Background
Many modern AI-powered CLI agents (such as Codex, Amazon Q Developer CLI, etc.) are primarily optimized for Linux and macOS systems. Windows users who want to experience these tools typically need to run them through WSL2 (Windows Subsystem for Linux 2). However, WSL2 has incomplete support for certain features, with **image pasting** being a notable pain point:

- **Problem**: WSL2 terminals cannot directly access image data from the Windows clipboard
- **Impact**: Users cannot paste screenshots directly to AI tools for analysis, unlike on native Linux/macOS
- **Workaround**: Some AI CLI tools (like Amazon Q Developer CLI) work around this by using a "save image to file → pass file path" approach

#### Solution
This tool is designed to bridge this gap: pressing `Alt+V` automatically saves the Windows clipboard image to a local file and pastes the corresponding WSL path (`/mnt/c/...`) into the active window, enabling AI tools to seamlessly read the image.

**v2.0 Enhancements**: While keeping the familiar `Alt+V` workflow, this version adds input method protection, asynchronous saves, automatic cleanup, and improved tray controls so that WSL-ready paths appear instantly in your terminal. By skipping upfront SHA256 deduplication and deferring disk I/O, the new flow pastes the path in under a second without the character-by-character delay that previously took roughly three seconds.

### Highlights
- **Instant Path Output**: Paste the `/mnt/...` path immediately after `Alt+V`, trimming end-to-end latency from ~3 seconds to under 1 second and avoiding the prior character-by-character send effect.
- **Input Method Safeguard**: Temporarily switch to the English keyboard layout to avoid IME mis-typing, then restore the prior layout.
- **Asynchronous Save Pipeline**: Offload image persistence to PowerShell in the background with silent error handling, keeping the hotkey responsive.
- **Automatic Cleanup**: Periodically prune cached images older than two hours and remove leftovers when exiting from the tray.
- **Enhanced Tray Menu**: Quickly open the cache directory or exit the helper directly from the system tray.

### Requirements
- Windows 10/11 with WSL2 enabled
- PowerShell 5.1+ with local script execution allowed (`Set-ExecutionPolicy RemoteSigned -Scope CurrentUser`)
- AutoHotkey v2 (already compiled into `wsl_clipboard.exe`; install only if you need to rebuild or debug)

### Usage
1. Clone the repository and move into the project folder:
   ```bash
   git clone https://github.com/cpulxb/WSL-Image-Clipboard-Helper.git
   cd WSL-Image-Clipboard-Helper
   ```
2. Keep `wsl_clipboard.exe` and its companion `.ps1` scripts together inside the `scripts` directory.
3. Double-click `scripts/wsl_clipboard.exe`; the helper minimizes to the system tray.
4. Press `Alt+V` in any editable field:
   - The clipboard image is stored in `temp/` asynchronously.
   - The `/mnt/...` path is pasted right away into the active window.
5. Use the tray icon menu → `Exit` to shut down gracefully; this triggers `exit-all.ps1` to clean processes and cached files.

### Notes
- `Alt+V` is a global hotkey; adjust it inside `scripts/wsl_clipboard.ahk` and rebuild if you encounter conflicts.
- If the tray icon is hidden, look in the taskbar overflow section.
- Save PowerShell scripts as UTF-8 with BOM when they contain non-ASCII characters to avoid parsing issues.
- You can run `scripts/exit-all.ps1` manually for a quick cleanup at any time.

### Rebuild (Optional)

If you want to customize the hotkey, modify the temp directory path, or distribute a new `.exe`, install AutoHotkey v2 first and use the bundled Ahk2Exe compiler:

1. **Install AutoHotkey v2**
   - Download and install the official [AutoHotkey v2](https://www.autohotkey.com/download/ahk-v2.exe)

2. **Modify the Script (Optional)**
   - **Change hotkey**: Edit `scripts/wsl_clipboard.ahk` line 18, change `!v::` to another key combination
     - `!v` = Alt+V
     - `^!v` = Ctrl+Alt+V
     - `^+v` = Ctrl+Shift+V
   - **Change temp directory**: Edit line 5, modify the `gTempDir` variable path
   - **Change cleanup interval**: Edit line 125, adjust the time parameter (default 2 hours = 7200000 milliseconds)

3. **Compile to Executable**
   - Launch `C:\Program Files\AutoHotkey\Compiler\Ahk2Exe.exe`
   - **Source (script file)**: Select `scripts\wsl_clipboard.ahk`
   - **Destination (.exe file)**: Specify output path (e.g., `scripts\wsl_clipboard.exe`)
   - **Base File (.bin, .exe)**: Choose appropriate base (recommended `AutoHotkey64.exe`)
   - Click `Convert` to start compilation

4. **Test the New Version**
   - Exit the old version from the tray first
   - Double-click the newly compiled `wsl_clipboard.exe` to test

### Additional Resources
- [Architecture & Workflow Details](docs/architecture_by_codex.md)

### Changelog

#### v2.0 (Current)
- ✨ **Path-first async save**: Paste path immediately, save image in background, latency reduced from ~3s to <1s
- 🔤 **IME protection**: Auto-switch to English input during paste, restore after
- 🧹 **Auto cleanup**: Remove images older than 2 hours every 2 hours
- 🚀 **Code simplification**: PowerShell scripts reduced from 86 to 28 lines (-67%)
- 🐛 **Encoding fix**: exit-all.ps1 now uses UTF-8 with BOM for emoji and Chinese characters
- ❌ **Cache removal**: Deleted last_output.txt, last_seq.txt, last_hash.txt

#### v1.0
- Basic clipboard image sync functionality
- SHA256 deduplication mechanism
- Cache file management

# WSL Image Clipboard Helper for ClaudeCode / Codex

Language: [中文说明](#中文说明) | [English Guide](#english-guide)

---

## 中文说明

### 概述

该工具用于在 Windows 中配合 WSL 环境下的 Claude Code、CodeX 等 AI 工具，快速保存剪贴板图片并将其转换为 WSL 路径，方便粘贴给 AI 读取。

### 必备环境
- Windows 10/11，已启用 WSL
- PowerShell 允许执行本地脚本（建议执行 `Set-ExecutionPolicy RemoteSigned -Scope CurrentUser`）
- （可选）如需重新编译或直接运行 `.ahk` 脚本，请安装 [AutoHotkey v2 官方版](https://www.autohotkey.com/download/ahk-v2.exe)

### 使用方式
1. 确保 `wsl_clipboard.exe` 与 `scripts` 目录（包含 `save-clipboard-image.ps1`、`exit-all.ps1` 等）在同一层级。
2. 双击 `wsl_clipboard.exe`，程序会最小化到系统托盘常驻后台。
3. 在任意输入框按下 `Alt+V`，剪贴板中的图片会保存到 `temp` 目录，并将对应的 `/mnt/...` 路径粘贴到当前窗口。
4. 需要退出时，在托盘图标上点击并选择 `Exit`，程序会自动调用 `exit-all.ps1` 清理子进程与缓存后退出。

### 常见注意事项
- `Alt+V` 是全局热键，可能与其他软件的快捷键冲突。若冲突，请修改 `scripts\wsl_clipboard.ahk` 中的热键并重新编译。
- 如果托盘图标没有及时出现，可在任务栏的隐藏图标列表中查看。
- 支持手动运行 `scripts\exit-all.ps1` 以清理所有相关进程和缓存文件。

### 重新编译（可选）
如需自定义热键或分发新的 `.exe`，需先安装 AutoHotkey v2，然后使用自带的 Ahk2Exe：
1. 打开 `C:\Program Files\AutoHotkey\Compiler\Ahk2Exe.exe`
2. 选择 `scripts\wsl_clipboard.ahk` 作为源文件
3. 指定合适的 Base（二进制）和输出路径，点击 `Convert`

---

## English Guide

### Overview
This helper targets Windows users running AI tools (Claude Code, Codex, etc.) inside WSL. Pressing `Alt+V` saves the clipboard image and pastes the corresponding WSL-friendly file path into the active window.

### Requirements
- Windows 10/11 with WSL enabled
- PowerShell allowed to run local scripts (run `Set-ExecutionPolicy RemoteSigned -Scope CurrentUser` if needed)
- (Optional) Install the official [AutoHotkey v2](https://www.autohotkey.com/download/ahk-v2.exe) if you plan to edit the source `.ahk` or rebuild the executable

### Usage
1. Keep `wsl_clipboard.exe` and the `scripts` folder (`save-clipboard-image.ps1`, `exit-all.ps1`, etc.) side by side.
2. Double-click `wsl_clipboard.exe`; it stays minimized in the system tray.
3. Press `Alt+V` in any input field. The clipboard image is written into the `temp` folder and its `/mnt/...` path is pasted where you're typing.
4. To quit, open the tray icon menu and click `Exit`; this runs `exit-all.ps1` to clean up helper processes and caches before closing.

### Notes
- `Alt+V` is a global hotkey and may conflict with other applications. If so, adjust the hotkey in `scripts\wsl_clipboard.ahk` and rebuild.
- If the tray icon is hidden, check the taskbar overflow area.
- You can run `scripts\exit-all.ps1` manually to reset caches and terminate helper processes.

### Rebuild (Optional)
If you want to customize the hotkey or rebuild the helper, install AutoHotkey v2 and use Ahk2Exe:
1. Launch `C:\Program Files\AutoHotkey\Compiler\Ahk2Exe.exe`
2. Select `scripts\wsl_clipboard.ahk` as the source
3. Pick the desired base binary and output path, then click `Convert`

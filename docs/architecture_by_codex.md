# 技术架构与流程说明 (WSL Clipboard Sync)

Language: [中文说明](#中文说明) | [English Version](#english-version)

---

## 中文说明

本补充文档详细介绍新版 WSL Clipboard Sync 的组件分工、执行流程与实现要点，便于维护者理解核心改动的价值。

### 组件职责

```
┌─────────────────────────────────────────────────────────────┐
│                    wsl_clipboard.exe (AHK)                  │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  • Alt+V 热键监听                                     │   │
│  │  • 输入法状态保存与恢复                               │   │
│  │  • 路径转换 (Windows → WSL)                          │   │
│  │  • 托盘菜单与状态通知                                 │   │
│  │  • 定时清理调度                                       │   │
│  └──────────────────────────────────────────────────────┘   │
│                           │                                  │
│                           ├─────────────┬───────────────┐    │
│                           ▼             ▼               ▼    │
│              ┌─────────────────┐  ┌──────────┐  ┌─────────┐ │
│              │ save-clipboard- │  │ exit-    │  │  temp/  │ │
│              │   image.ps1     │  │ all.ps1  │  │  *.png  │ │
│              │                 │  │          │  │         │ │
│              │ • 异步保存图片  │  │ • 退出   │  │ • 图片  │ │
│              │ • 错误静默处理  │  │   清理   │  │   缓存  │ │
│              └─────────────────┘  │ • 清理   │  └─────────┘ │
│                                   │   临时   │              │
│                                   │   文件   │              │
│                                   └──────────┘              │
└─────────────────────────────────────────────────────────────┘
```

### 核心流程

#### 图片粘贴流程

```
用户按下 Alt+V
    │
    ├─→ 保存当前输入法状态
    │
    ├─→ 切换到英文输入法
    │
    ├─→ 生成时间戳文件名 (yyyyMMdd_HHmmss.png)
    │
    ├─→ 将 Windows 路径转换为 WSL 路径
    │
    ├─→ 立即粘贴 WSL 路径到当前窗口
    │
    ├─→ 异步调用 PowerShell 保存图片
    │
    └─→ 延时恢复原输入法布局
```

#### 路径转换策略

```ahk
ConvertPathToWsl(winPath) {
    local p := Trim(winPath, '"')
    
    ; 处理常见的驱动器路径 "C:\path\to\file"
    if RegExMatch(p, "^[A-Za-z]:\\") {
        local drive := SubStr(p, 1, 1)
        local rest := SubStr(p, 3)
        rest := StrReplace(rest, "\", "/")
        rest := RegExReplace(rest, "^/+", "")
        return "/mnt/" . StrLower(drive) . "/" . rest
    }
    
    ; 如果不是驱动器路径，尝试调用 wsl wslpath
    try {
        local out := Trim(RunGetStdOut('wsl wslpath -a -u "' p '"'))
        if (out != "") {
            return out
        }
    } catch {
        ; 忽略错误
    }
    
    return ""
}
```

#### 异步保存设计

- 路径粘贴与图片保存解耦：前者独立完成，保证热键响应在 10ms 级别。
- PowerShell 负责读取剪贴板并保存图片文件，失败时写入日志但不会阻塞主流程。
- 保存命令运行在隐藏窗口，避免打断用户。

#### 定时与退出清理

- AHK 定时器每 2 小时扫描 `temp/`，删除超过阈值的图片，保持缓存轻量。
- 托盘 `Exit` 操作调用 `exit-all.ps1`，负责终止剩余脚本进程并清除缓存目录。

### 技术要点

- **输入法保护**：通过 `LoadKeyboardLayoutW` 预加载英文布局（HKL 0x00000409），并使用 `PostMessage(0x50, ...)` 切换，降低与前台应用的竞争。
- **编码策略**：包含中文或 emoji 的 PowerShell 文件必须使用 UTF-8 with BOM，因为 PowerShell 解析器依赖 BOM 来正确识别 UTF-8 编码；若全为 ASCII，可保持无 BOM。
- **错误容错**：异步脚本内部捕获异常，避免弹窗；必要时可扩展成日志文件或托盘通知。
- **路径转换优先级**：优先使用内置正则匹配（快速），失败时回退到 `wsl wslpath` 命令（兼容性）。

### 性能体验对比

- **旧版流程**：执行 SHA256 去重、等待图片保存完成、使用 `Send {Raw}` 逐字符回显，整体延迟约 3 秒。
- **新版流程**：路径粘贴与图片写入并行，避开前置校验与同步 I/O，直接调用 `SendText` 一次性输出，常见场景下路径在 1 秒内可用。
- 兼容性提示：如需恢复去重策略，可在异步 PowerShell 中补充校验逻辑，但可能重新引入延迟。

---

## English Version

This add-on document explains the component layout, execution flow, and implementation details of the revamped WSL Clipboard Sync so maintainers can assess the value of the changes quickly.

### Component Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    wsl_clipboard.exe (AHK)                  │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  • Alt+V hotkey listener                             │   │
│  │  • Input method state capture & restore              │   │
│  │  • Path conversion (Windows → WSL)                   │   │
│  │  • Tray menu & status notifications                  │   │
│  │  • Scheduled cleanup dispatcher                      │   │
│  └──────────────────────────────────────────────────────┘   │
│                           │                                  │
│                           ├─────────────┬───────────────┐    │
│                           ▼             ▼               ▼    │
│              ┌─────────────────┐  ┌──────────┐  ┌─────────┐ │
│              │ save-clipboard- │  │ exit-    │  │  temp/  │ │
│              │   image.ps1     │  │ all.ps1  │  │  *.png  │ │
│              │                 │  │          │  │         │ │
│              │ • Async image   │  │ • Exit   │  │ • Image │ │
│              │   persistence   │  │   cleanup │ │   cache │ │
│              │ • Silent errors │  │ • Temp    │ │         │ │
│              └─────────────────┘  │   purge   │  └─────────┘ │
│                                   │            │              │
│                                   └────────────┘              │
└─────────────────────────────────────────────────────────────┘
```

### Core Flow

#### Image Paste Sequence

```
Press Alt+V
    │
    ├─→ Save current input method handle
    │
    ├─→ Switch to the English layout
    │
    ├─→ Generate timestamped filename (yyyyMMdd_HHmmss.png)
    │
    ├─→ Convert Windows path to a WSL path
    │
    ├─→ Paste the WSL path immediately into the active window
    │
    ├─→ Trigger PowerShell asynchronously to write the image
    │
    └─→ Restore the original input method after a short delay
```

#### Path Conversion Strategy

```ahk
ConvertPathToWsl(winPath) {
    local p := Trim(winPath, '"')
    
    ; Handle common drive paths "C:\path\to\file"
    if RegExMatch(p, "^[A-Za-z]:\\") {
        local drive := SubStr(p, 1, 1)
        local rest := SubStr(p, 3)
        rest := StrReplace(rest, "\", "/")
        rest := RegExReplace(rest, "^/+", "")
        return "/mnt/" . StrLower(drive) . "/" . rest
    }
    
    ; Fallback to wsl wslpath command
    try {
        local out := Trim(RunGetStdOut('wsl wslpath -a -u "' p '"'))
        if (out != "") {
            return out
        }
    } catch {
        ; Ignore errors
    }
    
    return ""
}
```

#### Asynchronous Save Design

- Decouple path output from image persistence so the hotkey responds within roughly 10 ms.
- PowerShell is responsible for reading the clipboard and saving the file; failures are logged silently without blocking the main script.
- Commands run in hidden windows to avoid interrupting the user workflow.

#### Scheduled & Exit Cleanup

- An AutoHotkey timer scans `temp/` every two hours and deletes expired files to keep the cache light.
- Selecting `Exit` from the tray invokes `exit-all.ps1`, which terminates helper processes and clears the cache directory.

### Technical Highlights

- **Input Method Protection**: Preload the English layout (HKL 0x00000409) via `LoadKeyboardLayoutW` and switch with `PostMessage(0x50, ...)` to reduce contention with the foreground app.
- **Encoding Strategy**: Save PowerShell files that contain Chinese or emoji as UTF-8 with BOM, as PowerShell's parser relies on BOM to correctly identify UTF-8 encoding; ASCII-only files can remain without BOM.
- **Error Handling**: Async scripts capture exceptions and suppress pop-ups; optional logging or tray notifications can be layered in later.
- **Path Conversion Priority**: Use built-in regex matching first (fast), fallback to `wsl wslpath` command (compatibility).

### Performance Comparison

- **Previous Flow**: Performed SHA256 deduplication, waited for image persistence, and used `Send {Raw}` to emit characters one by one—total latency around three seconds.
- **Current Flow**: Runs path pasting and disk writes in parallel, avoids upfront validation and synchronous I/O, and calls `SendText` for a single-shot output—paths are typically available in under one second.
- Compatibility note: If deduplication is required again, it can be reintroduced inside the PowerShell script, keeping in mind it will likely reintroduce the earlier delay.


---

## 已知限制与扩展方向

### 中文

- 当前仅输出 `/mnt/...` 路径，如需 Windows 原生路径，可扩展 AHK 脚本提供模式切换。
- 现有清理策略按时间阈值执行，可考虑增加容量上限或自定义策略。
- 若未来支持多种热键模式，可在托盘菜单增加配置入口或引入 GUI 选项。

### English

- Currently only outputs `/mnt/...` paths; if Windows native paths are needed, extend the AHK script to provide mode switching.
- Current cleanup strategy uses time thresholds; consider adding capacity limits or custom policies.
- If multiple hotkey modes are needed in the future, add configuration entry in tray menu or introduce GUI options.

#Requires AutoHotkey v2.0
#SingleInstance Force

global gScriptDir := A_ScriptDir
global gTempDir := GetFullPath(gScriptDir "\..\temp")
global gPsScript := gScriptDir "\save-clipboard-image.ps1"
global gSetAttachmentClipboardScript := gScriptDir "\set-clipboard-attachment.ps1"
global gSaveAndSetAttachmentScript := gScriptDir "\save-and-set-attachment.ps1"
global gExitScript := gScriptDir "\exit-all.ps1"
global gConfigFile := gScriptDir "\wsl_clipboard.ini"
global gCleanupDone := false
global gLastNotifyAt := 0
global gCurrentHotkey := ""
global gHotkeyMenu := ""
global gSpeedModeMenu := ""
global gSpeedMode := "safe"
global gPasteFormatMenu := ""
global gPasteFormat := "plain"
global gPendingRestoreHKL := 0
global gWslTempDir := ""
global gLastImageClipboardSeq := 0
global gLastImageWinPath := ""
global gLastImageWslPath := ""
global gPendingSaveWinPath := ""
global gPendingSaveStartedAt := 0

; 缓存英文输入法 HKL（en-US）
global gEngHKL := DllCall("user32.dll\LoadKeyboardLayoutW", "WStr", "00000409", "UInt", 0x1, "UPtr")

SetupTrayMenu()
InitializeHelperScripts()
InitializeSpeedMode()
InitializePasteFormat()
InitializeHotkey()
OnExit(HandleExit)

#HotIf gCurrentHotkey = "!v"
!v::HandlePasteHotkey()
#HotIf gCurrentHotkey = "^!v"
^!v::HandlePasteHotkey()
#HotIf gCurrentHotkey = "!Enter"
!Enter::HandlePasteHotkey()
#HotIf

; ------------------ 粘贴热键：路径优先 + 输入法保护 ------------------
HandlePasteHotkey(*) {
    global gTempDir, gPsScript, gEngHKL
    local withImeGuard := IsImeGuardEnabled()
    local attachmentMode := IsAttachmentPasteMode()
    local prevHKL := 0
    local clipSeq := GetClipboardSequence()

    RefreshPendingSaveState()

    ; 剪贴板中没有图片时，回退到普通粘贴，避免无效路径输出
    if !ClipboardHasImage() {
        NormalizeModifierStateBeforeSend()
        Send("^v")
        return
    }

    ; 稳定模式下启用输入法保护；极速模式跳过此步骤以降低触发延迟
    if (withImeGuard) {
        ; 1) 获取当前活动窗口和线程 ID
        local hwnd := WinActive("A")
        local threadId := hwnd ? DllCall("user32.dll\GetWindowThreadProcessId", "UInt", hwnd, "UInt", 0, "UInt") : 0

        ; 2) 保存当前键盘布局
        prevHKL := threadId ? DllCall("user32.dll\GetKeyboardLayout", "UInt", threadId, "UPtr") : 0

        ; 3) 切换到英文输入法（仅在布局不同的时候切换，减少切换开销）
        if (gEngHKL != 0 && prevHKL != gEngHKL) {
            PostMessage(0x50, 0, gEngHKL, , "A")
            Sleep 60
        }
    }

    ; 4) 附件模式下，若剪贴板未变化且缓存文件仍存在，则直接复用，避免重复落盘
    if attachmentMode && TryPasteCachedAttachment(clipSeq) {
        if (withImeGuard) {
            ScheduleKeyboardLayoutRestore(prevHKL, 120)
        }
        return
    }
    if !attachmentMode && TryPasteCachedPlainPath(clipSeq) {
        if (withImeGuard) {
            ScheduleKeyboardLayoutRestore(prevHKL, 120)
        }
        return
    }

    ; 5) 生成高精度文件名，避免同一秒内触发导致文件名冲突
    local fileName := FormatTime(, "yyyyMMdd_HHmmss") "_" Format("{:03}", A_MSec + 0) ".png"
    local winPath := gTempDir "\" fileName

    ; 6) 附件通道（实验）：无需 @，优先触发客户端附件渲染
    if attachmentMode {
        if TryPasteAsAttachment(winPath) {
            SaveImageCache(winPath)
            ; 附件通道已完成，后续无需再发送文本或异步保存
            if (withImeGuard) {
                ScheduleKeyboardLayoutRestore(prevHKL, 120)
            }
            return
        }
        ; 附件通道失败时，回退到纯路径（兼容）避免无响应
        NotifyUser("附件模式回退", "附件通道失败，已回退为纯路径。", 2500, false)
    }

    ; 7) 仅在需要文本路径时再计算 WSL 路径（减少附件成功路径上的开销）
    local wslPath := BuildWslPath(winPath, fileName)
    if (wslPath = "") {
        if (withImeGuard) {
            RestoreKeyboardLayout(prevHKL)
        }
        NormalizeModifierStateBeforeSend()
        Send("^v")
        return
    }

    ; 8) 备份剪贴板（含图片），写入文本路径后用 Ctrl+V 瞬间粘贴，再恢复剪贴板
    local pasteText := BuildPasteText(wslPath)
    SavePlainPathCache(clipSeq, winPath, wslPath)

    local savedClip := ClipboardAll()
    A_Clipboard := pasteText
    NormalizeModifierStateBeforeSend()
    Send("^v")
    Sleep 80
    A_Clipboard := savedClip
    savedClip := ""

    ; 9) 异步调用 PowerShell 保存图片（不阻塞，剪贴板已恢复）
    if !TriggerAsyncSaveIfNeeded(winPath) {
        ; 路径已经粘贴了，但保存失败时给用户一个温和提示
        NotifyUser("图片保存失败", "路径已粘贴，但图片未保存。", 3000, true)
    }

    ; 10) 稳定模式下延迟恢复输入法，不阻塞主热键线程
    if (withImeGuard) {
        ScheduleKeyboardLayoutRestore(prevHKL, 120)
    }
}

BuildSaveCommand(winPath) {
    global gPsScript
    return 'powershell -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File "' gPsScript '" -FilePath "' winPath '"'
}

BuildSetAttachmentClipboardCommand(winPath) {
    global gSetAttachmentClipboardScript
    return 'powershell -NoLogo -NoProfile -NonInteractive -STA -ExecutionPolicy Bypass -WindowStyle Hidden -File "' gSetAttachmentClipboardScript '" -FilePath "' winPath '"'
}

BuildSaveAndSetAttachmentCommand(winPath) {
    global gSaveAndSetAttachmentScript
    return 'powershell -NoLogo -NoProfile -NonInteractive -STA -ExecutionPolicy Bypass -WindowStyle Hidden -File "' gSaveAndSetAttachmentScript '" -FilePath "' winPath '"'
}

TryPasteAsAttachment(winPath) {
    ; 附件通道性能优化：单进程完成“保存图片 + 设置附件剪贴板”
    if !SaveAndSetAttachmentSync(winPath) {
        return false
    }

    NormalizeModifierStateBeforeSend()
    Send("^v")
    return true
}

TryPasteCachedAttachment(clipSeq) {
    global gLastImageClipboardSeq, gLastImageWinPath
    if (clipSeq = 0 || clipSeq != gLastImageClipboardSeq) {
        return false
    }
    if (gLastImageWinPath = "" || !FileExist(gLastImageWinPath)) {
        return false
    }

    if !SetClipboardAttachmentSync(gLastImageWinPath) {
        return false
    }

    RefreshImageCacheSequence()
    NormalizeModifierStateBeforeSend()
    Send("^v")
    return true
}

TryPasteCachedPlainPath(clipSeq) {
    global gLastImageClipboardSeq, gLastImageWinPath, gLastImageWslPath
    if (clipSeq = 0 || clipSeq != gLastImageClipboardSeq) {
        return false
    }
    if (gLastImageWinPath = "") {
        return false
    }

    local wslPath := gLastImageWslPath
    if (wslPath = "") {
        local fileName := ""
        SplitPath(gLastImageWinPath, &fileName)
        wslPath := BuildWslPath(gLastImageWinPath, fileName)
    }
    if (wslPath = "") {
        return false
    }

    NormalizeModifierStateBeforeSend()
    SendText(wslPath)
    if !FileExist(gLastImageWinPath) {
        TriggerAsyncSaveIfNeeded(gLastImageWinPath)
    }
    return true
}

SaveImageCache(winPath) {
    global gLastImageClipboardSeq, gLastImageWinPath, gLastImageWslPath
    if FileExist(winPath) {
        gLastImageWinPath := winPath
        gLastImageWslPath := ""
        RefreshImageCacheSequence()
        return
    }
    gLastImageClipboardSeq := 0
    gLastImageWinPath := ""
    gLastImageWslPath := ""
}

SavePlainPathCache(clipSeq, winPath, wslPath) {
    global gLastImageClipboardSeq, gLastImageWinPath, gLastImageWslPath
    if (clipSeq = 0 || winPath = "" || wslPath = "") {
        return
    }
    gLastImageClipboardSeq := clipSeq
    gLastImageWinPath := winPath
    gLastImageWslPath := wslPath
}

GetClipboardSequence() {
    return DllCall("user32.dll\GetClipboardSequenceNumber", "UInt")
}

RefreshImageCacheSequence() {
    global gLastImageClipboardSeq
    local seq := GetClipboardSequence()
    gLastImageClipboardSeq := seq != 0 ? seq : 0
}

RefreshPendingSaveState() {
    global gPendingSaveWinPath, gPendingSaveStartedAt
    if (gPendingSaveWinPath = "") {
        return
    }

    ; 文件已落盘则清除“进行中”标记。
    if FileExist(gPendingSaveWinPath) {
        gPendingSaveWinPath := ""
        gPendingSaveStartedAt := 0
        return
    }

    ; 超过 10 秒仍未成功，认为上次异步保存异常，允许重试。
    if (gPendingSaveStartedAt != 0 && A_TickCount - gPendingSaveStartedAt > 10000) {
        gPendingSaveWinPath := ""
        gPendingSaveStartedAt := 0
    }
}

TriggerAsyncSaveIfNeeded(winPath) {
    global gPendingSaveWinPath, gPendingSaveStartedAt
    if (winPath = "") {
        return false
    }

    RefreshPendingSaveState()
    if FileExist(winPath) {
        return true
    }
    if (gPendingSaveWinPath = winPath) {
        return true
    }

    try {
        Run(BuildSaveCommand(winPath), "", "Hide")
        gPendingSaveWinPath := winPath
        gPendingSaveStartedAt := A_TickCount
        return true
    } catch {
        return false
    }
}

SaveAndSetAttachmentSync(winPath) {
    global gSaveAndSetAttachmentScript
    if !FileExist(gSaveAndSetAttachmentScript) {
        ; 脚本缺失时降级到旧流程，保证可用性
        if !SaveClipboardImageSync(winPath) {
            return false
        }
        return SetClipboardAttachmentSync(winPath)
    }

    try {
        RunWait(BuildSaveAndSetAttachmentCommand(winPath), "", "Hide")
    } catch {
        return false
    }
    return FileExist(winPath) != ""
}

SaveClipboardImageSync(winPath) {
    try {
        RunWait(BuildSaveCommand(winPath), "", "Hide")
    } catch {
        return false
    }
    return FileExist(winPath) != ""
}

SetClipboardAttachmentSync(winPath) {
    global gSetAttachmentClipboardScript
    if !FileExist(gSetAttachmentClipboardScript) {
        return false
    }

    try {
        RunWait(BuildSetAttachmentClipboardCommand(winPath), "", "Hide")
        return true
    } catch {
        return false
    }
}

ClipboardHasImage() {
    ; CF_BITMAP=2, CF_DIB=8, CF_DIBV5=17
    return DllCall("user32.dll\IsClipboardFormatAvailable", "UInt", 2, "Int")
        || DllCall("user32.dll\IsClipboardFormatAvailable", "UInt", 8, "Int")
        || DllCall("user32.dll\IsClipboardFormatAvailable", "UInt", 17, "Int")
}

NormalizeModifierStateBeforeSend() {
    ; 释放修饰键状态，避免 Alt 组合键触发后输入框吞字符
    SendInput("{Alt up}{Ctrl up}{Shift up}")
}

; ------------------ 恢复键盘布局的辅助函数 ------------------
RestoreKeyboardLayout(hkl) {
    if (hkl != 0) {
        PostMessage(0x50, 0, hkl, , "A")
        Sleep 30
    }
}

ScheduleKeyboardLayoutRestore(hkl, delayMs := 120) {
    global gPendingRestoreHKL
    if (hkl = 0) {
        return
    }
    gPendingRestoreHKL := hkl
    SetTimer(RestoreKeyboardLayoutTask, -delayMs)
}

RestoreKeyboardLayoutTask() {
    global gPendingRestoreHKL
    if (gPendingRestoreHKL != 0) {
        RestoreKeyboardLayout(gPendingRestoreHKL)
        gPendingRestoreHKL := 0
    }
}

BuildWslPath(winPath, fileName) {
    global gWslTempDir
    if (gWslTempDir != "") {
        return gWslTempDir "/" fileName
    }
    return ConvertPathToWsl(winPath)
}

BuildPasteText(wslPath) {
    global gPasteFormat
    switch gPasteFormat {
        case "attachment":
            ; 附件通道失败时，回退到纯路径
            return wslPath
        default:
            return wslPath
    }
}

; ------------------ 退出时的清理逻辑（调用 exit-all.ps1） ------------------
HandleExit(ExitReason, ExitCode) {
    CleanupAndExit(False)
}

ExitFromTray(*) {
    CleanupAndExit(True)
}

GetFullPath(path) {
    bufSize := 260  ; MAX_PATH
    buf := Buffer(bufSize * 2)  ; 每个字符2字节（Unicode）
    DllCall("GetFullPathNameW", "Str", path, "UInt", bufSize, "Ptr", buf, "Ptr", 0)
    return StrGet(buf)
}


CleanupAndExit(shouldExit) {
    global gCleanupDone, gExitScript, gScriptDir, gTempDir
    if (!gCleanupDone && FileExist(gExitScript)) {
        gCleanupDone := true
        try {
            RunWait(Format('powershell -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File "{1}" -TempDir "{2}"', gExitScript, gTempDir), gScriptDir, "Hide")
        } catch {
            ; 清理脚本失败时静默忽略，确保主程序仍可退出
        }
    }
    if (shouldExit) {
        ExitApp()
    }
}


ShowTempFolder(*) {
    global gTempDir
    local tempDir := DirExist(gTempDir) ? gTempDir : A_ScriptDir
    Run(Format('explorer "{1}"', tempDir))
}


InitializeHelperScripts() {
    global gTempDir, gWslTempDir
    if !DirExist(gTempDir) {
        try {
            DirCreate(gTempDir)
        } catch {
            ; 创建失败时忽略
        }
    }

    ; 预计算 temp 目录的 WSL 路径，减少每次热键触发时的转换开销
    local convertedDir := ConvertPathToWsl(gTempDir)
    if (convertedDir != "") {
        gWslTempDir := RTrim(convertedDir, "/")
    }

    SetTimer(CleanupTempFolder, 2 * 60 * 60 * 1000)  ; 每两个小时执行一次

}

SetupTrayMenu() {
    global gHotkeyMenu, gSpeedModeMenu, gPasteFormatMenu
    A_IconTip := "WSL CLI Clipboard Helper"
    try A_TrayMenu.Delete()
    gHotkeyMenu := Menu()
    gHotkeyMenu.Add("Alt+V", UseHotkeyAltV)
    gHotkeyMenu.Add("Ctrl+Alt+V", UseHotkeyCtrlAltV)
    gHotkeyMenu.Add("Alt+Enter", UseHotkeyAltEnter)
    gSpeedModeMenu := Menu()
    gSpeedModeMenu.Add("稳定模式（输入法保护）", UseSpeedModeSafe)
    gSpeedModeMenu.Add("极速模式（更快）", UseSpeedModeFast)
    gPasteFormatMenu := Menu()
    gPasteFormatMenu.Add("附件通道（无@，实验）", UsePasteFormatAttachment)
    gPasteFormatMenu.Add("纯路径（兼容）", UsePasteFormatPlain)
    A_TrayMenu.Add("切换快捷键", gHotkeyMenu)
    A_TrayMenu.Add("运行模式", gSpeedModeMenu)
    A_TrayMenu.Add("粘贴格式", gPasteFormatMenu)
    A_TrayMenu.Add("打开图片缓存", ShowTempFolder)
    A_TrayMenu.Add()
    A_TrayMenu.Add("Exit", ExitFromTray)
}

InitializeSpeedMode() {
    global gSpeedMode
    local configuredMode := LoadSpeedModeConfig()
    if !IsSupportedSpeedMode(configuredMode) {
        configuredMode := "safe"
    }
    gSpeedMode := configuredMode
    SaveSpeedModeConfig(configuredMode)
    UpdateSpeedModeMenuState()
}

InitializePasteFormat() {
    global gPasteFormat
    local configuredFormat := LoadPasteFormatConfig()
    if !IsSupportedPasteFormat(configuredFormat) {
        configuredFormat := "plain"
    }
    gPasteFormat := configuredFormat
    SavePasteFormatConfig(configuredFormat)
    UpdatePasteFormatMenuState()
}

InitializeHotkey() {
    global gCurrentHotkey
    local configuredHotkey := LoadHotkeyConfig()
    if !IsSupportedHotkey(configuredHotkey) {
        configuredHotkey := "!v"
    }

    gCurrentHotkey := configuredHotkey
    SaveHotkeyConfig(configuredHotkey)
    UpdateHotkeyMenuState()
}

LoadSpeedModeConfig() {
    global gConfigFile
    try {
        return Trim(IniRead(gConfigFile, "Runtime", "Mode", "safe"))
    } catch {
        return "safe"
    }
}

SaveSpeedModeConfig(mode) {
    global gConfigFile
    try {
        IniWrite(mode, gConfigFile, "Runtime", "Mode")
    } catch {
        ; 配置写入失败时静默忽略，不影响主流程
    }
}

LoadPasteFormatConfig() {
    global gConfigFile
    try {
        return Trim(IniRead(gConfigFile, "Output", "Format", "plain"))
    } catch {
        return "plain"
    }
}

SavePasteFormatConfig(format) {
    global gConfigFile
    try {
        IniWrite(format, gConfigFile, "Output", "Format")
    } catch {
        ; 配置写入失败时静默忽略，不影响主流程
    }
}

LoadHotkeyConfig() {
    global gConfigFile
    try {
        return Trim(IniRead(gConfigFile, "Hotkey", "Paste", "!v"))
    } catch {
        return "!v"
    }
}

SaveHotkeyConfig(hotkey) {
    global gConfigFile
    try {
        IniWrite(hotkey, gConfigFile, "Hotkey", "Paste")
    } catch {
        ; 配置写入失败时静默忽略，不影响主流程
    }
}

SwitchHotkeyTo(targetHotkey) {
    global gCurrentHotkey
    if (targetHotkey = gCurrentHotkey) {
        return
    }

    gCurrentHotkey := targetHotkey
    SaveHotkeyConfig(targetHotkey)
    UpdateHotkeyMenuState()
    NotifyUser("快捷键已切换", "当前为 " HotkeyToLabel(targetHotkey), 1800)
}

SwitchSpeedModeTo(targetMode) {
    global gSpeedMode
    if (targetMode = gSpeedMode) {
        return
    }

    gSpeedMode := targetMode
    SaveSpeedModeConfig(targetMode)
    UpdateSpeedModeMenuState()
    NotifyUser("运行模式已切换", "当前为 " SpeedModeToLabel(targetMode), 2200)
}

UseHotkeyAltV(*) {
    SwitchHotkeyTo("!v")
}

UseHotkeyCtrlAltV(*) {
    SwitchHotkeyTo("^!v")
}

UseHotkeyAltEnter(*) {
    SwitchHotkeyTo("!Enter")
}

UseSpeedModeSafe(*) {
    SwitchSpeedModeTo("safe")
}

UseSpeedModeFast(*) {
    SwitchSpeedModeTo("fast")
}

UsePasteFormatPlain(*) {
    SwitchPasteFormatTo("plain")
}

UsePasteFormatAttachment(*) {
    SwitchPasteFormatTo("attachment")
}

SwitchPasteFormatTo(targetFormat) {
    global gPasteFormat
    if (targetFormat = gPasteFormat) {
        return
    }

    gPasteFormat := targetFormat
    SavePasteFormatConfig(targetFormat)
    UpdatePasteFormatMenuState()
    NotifyUser("粘贴格式已切换", "当前为 " PasteFormatToLabel(targetFormat), 2200)
}

UpdateHotkeyMenuState() {
    global gHotkeyMenu, gCurrentHotkey
    if !IsObject(gHotkeyMenu) {
        return
    }

    gHotkeyMenu.Uncheck("Alt+V")
    gHotkeyMenu.Uncheck("Ctrl+Alt+V")
    gHotkeyMenu.Uncheck("Alt+Enter")

    local currentLabel := HotkeyToLabel(gCurrentHotkey)
    if (currentLabel != "") {
        gHotkeyMenu.Check(currentLabel)
    }
    UpdateTrayIconTip()
}

UpdateSpeedModeMenuState() {
    global gSpeedModeMenu, gSpeedMode
    if !IsObject(gSpeedModeMenu) {
        return
    }

    gSpeedModeMenu.Uncheck("稳定模式（输入法保护）")
    gSpeedModeMenu.Uncheck("极速模式（更快）")

    local modeLabel := SpeedModeToLabel(gSpeedMode)
    if (modeLabel != "") {
        gSpeedModeMenu.Check(modeLabel)
    }
    UpdateTrayIconTip()
}

UpdatePasteFormatMenuState() {
    global gPasteFormatMenu, gPasteFormat
    if !IsObject(gPasteFormatMenu) {
        return
    }

    gPasteFormatMenu.Uncheck("附件通道（无@，实验）")
    gPasteFormatMenu.Uncheck("纯路径（兼容）")

    local formatLabel := PasteFormatToLabel(gPasteFormat)
    if (formatLabel != "") {
        gPasteFormatMenu.Check(formatLabel)
    }
    UpdateTrayIconTip()
}

UpdateTrayIconTip() {
    global gCurrentHotkey, gSpeedMode, gPasteFormat
    local hotkeyLabel := HotkeyToLabel(gCurrentHotkey)
    local modeLabel := SpeedModeToShortLabel(gSpeedMode)
    local formatLabel := PasteFormatToShortLabel(gPasteFormat)

    local tipParts := ""
    if (hotkeyLabel != "") {
        tipParts := hotkeyLabel
    }
    if (modeLabel != "") {
        tipParts := tipParts = "" ? modeLabel : tipParts " | " modeLabel
    }
    if (formatLabel != "") {
        tipParts := tipParts = "" ? formatLabel : tipParts " | " formatLabel
    }

    if (tipParts = "") {
        A_IconTip := "WSL CLI Clipboard Helper"
        return
    }

    A_IconTip := "WSL CLI Clipboard Helper (" tipParts ")"
}

HotkeyToLabel(hotkey) {
    switch hotkey {
        case "!v":
            return "Alt+V"
        case "^!v":
            return "Ctrl+Alt+V"
        case "!Enter":
            return "Alt+Enter"
        default:
            return ""
    }
}

IsSupportedHotkey(hotkey) {
    return hotkey = "!v" || hotkey = "^!v" || hotkey = "!Enter"
}

IsSupportedPasteFormat(format) {
    return format = "attachment" || format = "plain"
}

PasteFormatToLabel(format) {
    switch format {
        case "attachment":
            return "附件通道（无@，实验）"
        case "plain":
            return "纯路径（兼容）"
        default:
            return ""
    }
}

PasteFormatToShortLabel(format) {
    switch format {
        case "attachment":
            return "附件"
        case "plain":
            return "路径"
        default:
            return ""
    }
}

SpeedModeToLabel(mode) {
    switch mode {
        case "safe":
            return "稳定模式（输入法保护）"
        case "fast":
            return "极速模式（更快）"
        default:
            return ""
    }
}

SpeedModeToShortLabel(mode) {
    switch mode {
        case "safe":
            return "稳定"
        case "fast":
            return "极速"
        default:
            return ""
    }
}

IsSupportedSpeedMode(mode) {
    return mode = "safe" || mode = "fast"
}

IsImeGuardEnabled() {
    global gSpeedMode
    return gSpeedMode != "fast"
}

IsAttachmentPasteMode() {
    global gPasteFormat
    return gPasteFormat = "attachment"
}

; ------------------ 工具函数：把 Windows 路径转换为 WSL 路径 ------------------
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


RunGetStdOut(cmd) {
    shell := ComObject("WScript.Shell")
    exec := shell.Exec(A_ComSpec " /C " cmd)
    return exec.StdOut.ReadAll()
}


; 通用提示函数：托盘提示 + 可选蜂鸣，带简单去抖避免刷屏
NotifyUser(title, msg, durationMs := 2500, beep := false) {
    global gLastNotifyAt
    now := A_TickCount
    if (now - gLastNotifyAt < 800)  ; 800ms 去抖
        return
    gLastNotifyAt := now

    TrayTip(title, msg, durationMs)
    if (beep) {
        SoundBeep(750, 120)  ; 轻微提示音
    }
}


CleanupTempFolder(*) {
    global gTempDir, gLastImageWinPath, gLastImageClipboardSeq, gLastImageWslPath, gPendingSaveWinPath, gPendingSaveStartedAt
    try {
        if (gLastImageWinPath != "" && !FileExist(gLastImageWinPath)) {
            gLastImageWinPath := ""
            gLastImageClipboardSeq := 0
            gLastImageWslPath := ""
        }
        if (gPendingSaveWinPath != "" && FileExist(gPendingSaveWinPath)) {
            gPendingSaveWinPath := ""
            gPendingSaveStartedAt := 0
        }
        Loop Files gTempDir "\*.png", "F" {
            ; 新时间在前，旧时间在后，得到正的秒数差
            if (DateDiff(A_Now, A_LoopFileTimeModified, "Seconds") > 7200) {
                FileDelete(A_LoopFileFullPath)
            }
        }
    } catch {
        ; 忽略清理失败
    }
}

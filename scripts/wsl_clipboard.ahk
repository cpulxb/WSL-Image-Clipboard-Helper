#Requires AutoHotkey v2.0
#SingleInstance Force

global gScriptDir := A_ScriptDir
global gTempDir := GetFullPath(gScriptDir "\..\temp")
global gPsScript := gScriptDir "\save-clipboard-image.ps1"
global gExitScript := gScriptDir "\exit-all.ps1"
global gCleanupDone := false
global gLastNotifyAt := 0

; 缓存英文输入法 HKL（en-US）
global gEngHKL := DllCall("user32.dll\LoadKeyboardLayoutW", "WStr", "00000409", "UInt", 0x1, "UPtr")

SetupTrayMenu()
InitializeHelperScripts()
OnExit(HandleExit)

; ------------------ Alt+V 热键：路径优先 + 输入法保护 ------------------
!v:: {
    global gScriptDir, gPsScript
    
    ; 1) 获取当前活动窗口和线程ID
    local hwnd := WinActive("A")
    local threadId := DllCall("user32.dll\GetWindowThreadProcessId", "UInt", hwnd, "UInt", 0, "UInt")
    
    ; 2) 保存当前键盘布局
    local prevHKL := DllCall("user32.dll\GetKeyboardLayout", "UInt", threadId, "UPtr")
    
    ; 3) 切换到英文输入法（使用缓存的 gEngHKL）
    if (gEngHKL != 0) {
        PostMessage(0x50, 0, gEngHKL, , "A")
        Sleep 100  ; 等待切换完成
    }
    
    ; 4) 生成文件名和路径（基于时间戳）
    local fileName := FormatTime(, "yyyyMMdd_HHmmss") ".png"
    local winPath := gTempDir "\" fileName
    local wslPath := ConvertPathToWsl(winPath)
    
    ; 5) 如果没有有效 wslPath，回退到普通粘贴
    if (wslPath = "") {
        RestoreKeyboardLayout(prevHKL)
        Send("^v")
        return
    }
    
    ; 6) 立即粘贴 WSL 路径（此时已是英文输入法）
    ; PasteText(wslPath)
    SendText(wslPath)

    
    ; 7) 异步调用 PowerShell 保存图片（不阻塞）
    try {
        Run('powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' gPsScript '" -FilePath "' winPath '"', "", "Hide")
    } catch {
        ; 路径已经粘贴了，但保存失败时给用户一个温和提示
        NotifyUser("图片保存失败", "路径已粘贴，但图片未保存。", 3000, true)
    }
    
    ; 8) 恢复原来的键盘布局
    Sleep 500
    RestoreKeyboardLayout(prevHKL)
    return
}

; ------------------ 恢复键盘布局的辅助函数 ------------------
RestoreKeyboardLayout(hkl) {
    if (hkl != 0) {
        PostMessage(0x50, 0, hkl, , "A")
        Sleep 50
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
    global gCleanupDone, gExitScript, gScriptDir
    if (!gCleanupDone && FileExist(gExitScript)) {
        gCleanupDone := true
        try {
            RunWait(Format('powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "{1}" -TempDir "{2}"', gExitScript, gTempDir), gScriptDir, "Hide")
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
    global gTempDir
    if !DirExist(gTempDir) {
        try {
            DirCreate(gTempDir)
        } catch {
            ; 创建失败时忽略
        }
    }
    SetTimer(CleanupTempFolder, 2 * 60 * 60 * 1000)  ; 每两个小时执行一次

}

SetupTrayMenu() {
    A_IconTip := "WSL CLI Clipboard Helper"
    try A_TrayMenu.Delete()
    A_TrayMenu.Add("打开图片缓存", ShowTempFolder)
    A_TrayMenu.Add()
    A_TrayMenu.Add("Exit", ExitFromTray)
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
    global gTempDir
    try {
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



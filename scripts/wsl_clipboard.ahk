#Requires AutoHotkey v2.0
#SingleInstance Force

global gScriptDir := A_ScriptDir
global gPsScript := gScriptDir "\save-clipboard-image.ps1"
global gExitScript := gScriptDir "\exit-all.ps1"
global gCleanupDone := false

SetupTrayMenu()
InitializeHelperScripts()
OnExit(HandleExit)

; ------------------ Alt+V 热键：保存剪贴板图片 → 粘入 WSL 路径（含输入法/布局切换） ------------------
!v:: {
    ; --- 局部变量 ---
    local wslPath := ""
    local winPath := ""
    local lastOutFile := A_ScriptDir "\last_output.txt"
    local psScript := gPsScript
    
    ; 1) 获取当前活动窗口和线程ID
    local hwnd := WinActive("A")
    local threadId := DllCall("user32.dll\GetWindowThreadProcessId", "UInt", hwnd, "UInt", 0, "UInt")
    
    ; 2) 保存当前键盘布局
    local prevHKL := DllCall("user32.dll\GetKeyboardLayout", "UInt", threadId, "UPtr")
    
    ; 3) 切换到英文输入法 (en-US, 0x04090409)
    local engHKL := DllCall("user32.dll\LoadKeyboardLayoutW", "WStr", "00000409", "UInt", 0x1, "UPtr")
    if (engHKL != 0) {
        ; 向当前窗口发送切换布局消息
        PostMessage(0x50, 0, engHKL, , "A")
        Sleep 100  ; 等待切换完成
    }
    
    ; 4) 调用 PowerShell 脚本去保存剪贴板图片
    try {
        RunWait('powershell -NoProfile -ExecutionPolicy Bypass -File "' psScript '"', "", "Hide")
    } catch {
        ; 调用失败，恢复布局后回退到普通粘贴
        RestoreKeyboardLayout(prevHKL)
        Send("^v")
        return
    }
    
    ; 5) 读取 last_output.txt
    if FileExist(lastOutFile) {
        try {
            winPath := Trim(FileRead(lastOutFile))
        } catch {
            winPath := ""
        }
    } else {
        winPath := ""
    }
    
    ; 6) 转换成 WSL 路径
    if (winPath != "") {
        wslPath := ConvertPathToWsl(winPath)
    } else {
        wslPath := ""
    }
    
    ; 7) 如果没有有效 wslPath，回退到普通粘贴
    if (wslPath = "") {
        RestoreKeyboardLayout(prevHKL)
        Send("^v")
        return
    }
    
    ; 8) 粘贴 WSL 路径（此时已是英文输入法）
    Send("{Raw}" wslPath)
    Sleep 100
    
    ; 9) 恢复原来的键盘布局
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

CleanupAndExit(shouldExit) {
    global gCleanupDone, gExitScript, gScriptDir
    if (!gCleanupDone && FileExist(gExitScript)) {
        gCleanupDone := true
        try {
            RunWait(Format('powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "{1}"', gExitScript), gScriptDir, "Hide")
        } catch {
            ; 清理脚本失败时静默忽略，确保主程序仍可退出
        }
    }
    if (shouldExit) {
        ExitApp()
    }
}

ShowTempFolder(*) {
    global gScriptDir
    local tempDir := DirExist(gScriptDir "\..\temp") ? gScriptDir "\..\temp" : gScriptDir
    Run(Format('explorer "{1}"', tempDir))
}

InitializeHelperScripts() {
    global gPsScript, gScriptDir
    if !FileExist(gPsScript) {
        return
    }
    try {
        RunWait(Format('powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "{1}" -InitOnly', gPsScript), gScriptDir, "Hide")
    } catch {
        ; 启动预热失败时忽略，热键仍会在首次触发时调用 PowerShell 脚本
    }
}

SetupTrayMenu() {
    A_IconTip := "Claude Codex Clipboard Helper"
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
        local tmpOut := A_Temp "\wslpath_out.txt"
        FileDelete(tmpOut)
        RunWait('wsl wslpath -a -u "' p '" > "' tmpOut '" 2> nul', "", "Hide")
        if FileExist(tmpOut) {
            local out := Trim(FileRead(tmpOut))
            FileDelete(tmpOut)
            if (out != "") {
                return out
            }
        }
    } catch {
        ; 忽略错误
    }
    
    return ""
}

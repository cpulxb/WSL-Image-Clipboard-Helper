param(
    [string]$TempDir,
    [switch]$Verbose
)

Write-Host "🔧 正在退出 wsl_clipboard 插件..." -ForegroundColor Cyan

# 如果未传入 TempDir，则使用默认路径
if (-not $TempDir) {
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
    $TempDir = Join-Path (Split-Path -Parent $scriptDir) 'temp'
}

if ($Verbose) {
    Write-Host "📁 TempDir: $TempDir" -ForegroundColor DarkGray
}

# 获取所有进程（提前缓存）
$allProcs = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue

# 匹配 AutoHotkey 脚本进程（使用 ExecutablePath）
$ahkProcs = $allProcs | Where-Object {
    $_.ExecutablePath -and $_.ExecutablePath -like "*AutoHotkey*.exe" -and $_.ExecutablePath -like "*wsl_clipboard.ahk*"
}

if ($Verbose) {
    Write-Host "`n🔍 匹配到的 AutoHotkey 进程：" -ForegroundColor Cyan
    $ahkProcs | ForEach-Object {
        Write-Host " - PID=$($_.ProcessId) | $($_.ExecutablePath)" -ForegroundColor Gray
    }
}

foreach ($proc in $ahkProcs) {
    Write-Host "🧹 结束 AutoHotkey 进程 PID=$($proc.ProcessId)" -ForegroundColor Yellow
    Stop-Process -Id $proc.ProcessId -Force -ErrorAction SilentlyContinue
}

# 匹配编译版插件进程（wsl_clipboard.exe 或备份版本）
$helperExeProcs = $allProcs | Where-Object {
    $_.Name -and $_.Name -like "wsl_clipboard*.exe"
}

if ($Verbose) {
    Write-Host "`n🔍 匹配到的编译版插件进程：" -ForegroundColor Cyan
    $helperExeProcs | ForEach-Object {
        Write-Host " - PID=$($_.ProcessId) | $($_.Name) | $($_.ExecutablePath)" -ForegroundColor Gray
    }
}

foreach ($proc in $helperExeProcs) {
    Write-Host "🧹 结束编译版插件进程 PID=$($proc.ProcessId)" -ForegroundColor Yellow
    Stop-Process -Id $proc.ProcessId -Force -ErrorAction SilentlyContinue
}

# 匹配 PowerShell 剪贴板脚本进程（使用 CommandLine）
$psProcs = $allProcs | Where-Object {
    $_.CommandLine -and (
        $_.CommandLine -like "*save-clipboard-image.ps1*" -or
        $_.CommandLine -like "*save-and-set-attachment.ps1*" -or
        $_.CommandLine -like "*set-clipboard-attachment.ps1*"
    )
}

if ($Verbose) {
    Write-Host "`n🔍 匹配到的 PowerShell 剪贴板脚本进程：" -ForegroundColor Cyan
    $psProcs | ForEach-Object {
        Write-Host " - PID=$($_.ProcessId) | $($_.CommandLine)" -ForegroundColor Gray
    }
}

foreach ($proc in $psProcs) {
    Write-Host "🧹 结束 PowerShell 剪贴板脚本 PID=$($proc.ProcessId)" -ForegroundColor Yellow
    Stop-Process -Id $proc.ProcessId -Force -ErrorAction SilentlyContinue
}

# 清理临时目录
if (Test-Path $TempDir) {
    Write-Host "`n🗑️ 清理临时文件夹：$TempDir" -ForegroundColor Yellow
    Remove-Item -Path (Join-Path $TempDir '*') -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "`n✅ 所有相关进程与临时文件已清理完毕。" -ForegroundColor Green
Start-Sleep -Seconds 1
exit

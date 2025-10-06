Write-Host "🔧 正在退出 claude-codex 插件..." -ForegroundColor Cyan

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$baseDir = Split-Path -Parent $scriptDir

$ahkPattern = [Regex]::Escape((Join-Path $scriptDir 'wsl_clipboard.ahk'))
$psPattern = [Regex]::Escape((Join-Path $scriptDir 'save-clipboard-image.ps1'))

Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -match $ahkPattern } |
    ForEach-Object {
        Write-Host "🧹 结束 AutoHotkey 进程 PID=$($_.ProcessId)" -ForegroundColor Yellow
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }

Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -match $psPattern } |
    ForEach-Object {
        Write-Host "🧹 结束 PowerShell 剪贴板脚本 PID=$($_.ProcessId)" -ForegroundColor Yellow
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }

$tempPath = Join-Path $baseDir 'temp'
if (Test-Path $tempPath) {
    Write-Host "🗑️ 清理临时文件夹：$tempPath" -ForegroundColor Yellow
    Remove-Item -Path (Join-Path $tempPath '*') -Recurse -Force -ErrorAction SilentlyContinue
}

$cacheFiles = @('last_output.txt', 'last_seq.txt', 'last_hash.txt')
foreach ($f in $cacheFiles) {
    $path = Join-Path $scriptDir $f
    if (Test-Path $path) {
        Write-Host "🧽 删除缓存文件: $path" -ForegroundColor DarkGray
        Remove-Item -Path $path -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "✅ 所有相关脚本与缓存文件已清理完毕。" -ForegroundColor Green
Start-Sleep -Seconds 1
exit

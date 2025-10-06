param(
    [switch]$InitOnly
)

# This script saves clipboard images into the temp directory and caches metadata
# so that the AutoHotkey helper can reuse paths without creating duplicates.

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$baseDir = Split-Path -Parent $scriptDir

$tempDir = Join-Path $baseDir 'temp'
$outputFile = Join-Path $scriptDir 'last_output.txt'
$seqFile = Join-Path $scriptDir 'last_seq.txt'
$hashFile = Join-Path $scriptDir 'last_hash.txt'

if (-not (Test-Path $tempDir)) {
    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
}

if ($InitOnly) {
    exit
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class ClipboardHelper {
    [DllImport("user32.dll")]
    public static extern uint GetClipboardSequenceNumber();
}
"@
$currentSeq = [ClipboardHelper]::GetClipboardSequenceNumber()

$lastSeq = if (Test-Path $seqFile) { Get-Content $seqFile -Raw } else { $null }
if ($lastSeq -and ($lastSeq -eq $currentSeq)) {
    if (Test-Path $outputFile) {
        $lastPath = Get-Content $outputFile -Raw
        if ($lastPath -and (Test-Path $lastPath)) {
            Set-Content -Path $outputFile -NoNewline -Value $lastPath
            Write-Output $lastPath
            exit
        }
    }
}

Set-Content -Path $seqFile -Value $currentSeq -NoNewline

if ([System.Windows.Forms.Clipboard]::ContainsImage()) {
    $image = [System.Windows.Forms.Clipboard]::GetImage()

    $ms = New-Object System.IO.MemoryStream
    $image.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $ms.ToArray()
    $ms.Close()
    $hash = [BitConverter]::ToString((New-Object Security.Cryptography.SHA256Managed).ComputeHash($bytes))

    $lastHash = if (Test-Path $hashFile) { Get-Content $hashFile -Raw } else { $null }
    if ($lastHash -and ($lastHash -eq $hash)) {
        if (Test-Path $outputFile) {
            $lastPath = Get-Content $outputFile -Raw
            if ($lastPath -and (Test-Path $lastPath)) {
                Set-Content -Path $outputFile -NoNewline -Value $lastPath
                Write-Output $lastPath
                exit
            }
        }
    }

    $fileName = (Get-Date).ToString('yyyyMMdd_HHmmss') + '.png'
    $filePath = Join-Path $tempDir $fileName
    $image.Save($filePath, [System.Drawing.Imaging.ImageFormat]::Png)

    Set-Content -Path $hashFile -Value $hash -NoNewline
    Set-Content -Path $outputFile -Value $filePath -NoNewline
    Write-Output $filePath
}
elseif (Test-Path $outputFile) {
    $lastPath = Get-Content $outputFile -Raw
    if ($lastPath -and (Test-Path $lastPath)) {
        Set-Content -Path $outputFile -NoNewline -Value $lastPath
        Write-Output $lastPath
    }
}

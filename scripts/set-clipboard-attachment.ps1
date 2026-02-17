param(
    [string]$FilePath
)

if (-not $FilePath) {
    exit 1
}

if (-not (Test-Path -LiteralPath $FilePath)) {
    exit 1
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$img = $null
$bytes = $null
$pngStream = $null
try {
    $img = [System.Drawing.Image]::FromFile($FilePath)
    $bytes = [System.IO.File]::ReadAllBytes($FilePath)
    $pngStream = New-Object System.IO.MemoryStream(,$bytes)
} catch {
    exit 1
}

try {
    $data = New-Object System.Windows.Forms.DataObject

    # 图片格式：部分客户端通过位图通道识别为附件
    $data.SetData([System.Windows.Forms.DataFormats]::Bitmap, $true, $img)

    # PNG 原始流：部分 Electron/TUI 客户端会优先读取 PNG
    $data.SetData("PNG", $true, $pngStream)

    # 文件拖放格式：部分客户端通过文件通道识别为附件
    $files = New-Object System.Collections.Specialized.StringCollection
    [void]$files.Add($FilePath)
    $data.SetFileDropList($files)

    [System.Windows.Forms.Clipboard]::SetDataObject($data, $true)
    exit 0
} catch {
    exit 1
} finally {
    if ($pngStream -ne $null) {
        $pngStream.Dispose()
    }
    if ($img -ne $null) {
        $img.Dispose()
    }
}

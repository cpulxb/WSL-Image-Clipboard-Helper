param(
    [string]$FilePath
)

if (-not $FilePath) {
    exit 1
}

$dir = Split-Path -Parent $FilePath
if (-not $dir) {
    exit 1
}

try {
    if (-not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
} catch {
    exit 1
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

if (-not [System.Windows.Forms.Clipboard]::ContainsImage()) {
    exit 1
}

$image = $null
$pngBuffer = $null
$pngStream = $null
try {
    $image = [System.Windows.Forms.Clipboard]::GetImage()
    if ($null -eq $image) {
        exit 1
    }

    # 先在内存中编码 PNG，再一次写盘，避免“先写盘再读盘”的额外 IO。
    $pngBuffer = New-Object System.IO.MemoryStream
    $image.Save($pngBuffer, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $pngBuffer.ToArray()
    [System.IO.File]::WriteAllBytes($FilePath, $bytes)

    $pngStream = New-Object System.IO.MemoryStream(,$bytes)
    $data = New-Object System.Windows.Forms.DataObject

    # 图片通道：客户端若支持，会直接渲染为附件占位。
    $data.SetData([System.Windows.Forms.DataFormats]::Bitmap, $true, $image)
    $data.SetData("PNG", $true, $pngStream)

    # 文件通道：兼容通过文件粘贴识别附件的客户端。
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
    if ($pngBuffer -ne $null) {
        $pngBuffer.Dispose()
    }
    if ($image -ne $null) {
        $image.Dispose()
    }
}

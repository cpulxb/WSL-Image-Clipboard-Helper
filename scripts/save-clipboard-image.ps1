param(
    [string]$FilePath
)

# This script saves clipboard images to the specified file path.
# Called asynchronously by AHK, no need to return anything.

if (-not $FilePath) {
    exit
}

$dir = Split-Path -Parent $FilePath
if (-not (Test-Path $dir)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

if ([System.Windows.Forms.Clipboard]::ContainsImage()) {
    try {
        $image = [System.Windows.Forms.Clipboard]::GetImage()
        $image.Save($FilePath, [System.Drawing.Imaging.ImageFormat]::Png)
        $image.Dispose()
    } catch {
        # Save failed, silently ignore
    }
}

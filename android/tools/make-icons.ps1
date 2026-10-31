param(
    [string]$Source = "D:\codeX\meeting\src-tauri\icons\icon.png",
    [string]$ResDir = "D:\codeX\meeting\android\app\src\main\res"
)

Add-Type -AssemblyName System.Drawing

$src = [System.Drawing.Bitmap]::FromFile($Source)

# Opaque bounding box: icon.png has a transparent margin around the artwork.
$minX = $src.Width; $minY = $src.Height; $maxX = 0; $maxY = 0
$rect = New-Object System.Drawing.Rectangle(0, 0, $src.Width, $src.Height)
$data = $src.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$stride = $data.Stride
$bytes = New-Object byte[] ($stride * $src.Height)
[System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
$src.UnlockBits($data)
for ($y = 0; $y -lt $src.Height; $y++) {
    $row = $y * $stride
    for ($x = 0; $x -lt $src.Width; $x++) {
        if ($bytes[$row + $x * 4 + 3] -gt 8) {
            if ($x -lt $minX) { $minX = $x }
            if ($x -gt $maxX) { $maxX = $x }
            if ($y -lt $minY) { $minY = $y }
            if ($y -gt $maxY) { $maxY = $y }
        }
    }
}
$crop = New-Object System.Drawing.Rectangle($minX, $minY, ($maxX - $minX + 1), ($maxY - $minY + 1))
Write-Output "source $($src.Width)x$($src.Height), opaque box $crop"

# Edge color of the artwork: the adaptive background layer, the round icon's
# fill and the foreground's underlay, so no launcher mask ever exposes a seam.
$cx = [int](($crop.X + $crop.Width / 2))
$bg = $src.GetPixel($cx, $crop.Y + 8)
Write-Output ("background #{0:X2}{1:X2}{2:X2}" -f $bg.R, $bg.G, $bg.B)

function New-Canvas([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    return @($bmp, $g)
}

function Save-Png($bmp, [string]$path) {
    New-Item -ItemType Directory -Force -Path (Split-Path $path) | Out-Null
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
}

function Draw-Artwork($g, [int]$size) {
    # The full 512x512 sheet, unscaled in aspect: the Android icon is the
    # Windows icon, corner radius and all.
    $g.DrawImage($src, (New-Object System.Drawing.Rectangle(0, 0, $size, $size)), (New-Object System.Drawing.Rectangle(0, 0, $src.Width, $src.Height)), [System.Drawing.GraphicsUnit]::Pixel)
}

# Legacy launcher icon: the artwork with its own rounded corners, untouched.
$legacy = [ordered]@{ mdpi = 48; hdpi = 72; xhdpi = 96; xxhdpi = 144; xxxhdpi = 192 }
foreach ($d in $legacy.Keys) {
    $size = $legacy[$d]
    $pair = New-Canvas $size
    $bmp = $pair[0]; $g = $pair[1]
    Draw-Artwork $g $size
    $g.Dispose()
    Save-Png $bmp "$ResDir\mipmap-$d\ic_launcher.png"
    $bmp.Dispose()
}

# Round launcher icon: artwork clipped to a circle over the same edge color.
foreach ($d in $legacy.Keys) {
    $size = $legacy[$d]
    $pair = New-Canvas $size
    $bmp = $pair[0]; $g = $pair[1]
    $brush = New-Object System.Drawing.SolidBrush($bg)
    $g.FillEllipse($brush, 0, 0, ($size - 1), ($size - 1))
    $g.ResetClip()
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddEllipse(0, 0, ($size - 1), ($size - 1))
    $g.SetClip($path)
    Draw-Artwork $g $size
    $g.ResetClip()
    $path.Dispose()
    $brush.Dispose()
    $g.Dispose()
    Save-Png $bmp "$ResDir\mipmap-$d\ic_launcher_round.png"
    $bmp.Dispose()
}

# Adaptive foreground: the artwork full-bleed over the edge colour. A launcher
# mask trims the corners, which is exactly where the artwork is transparent.
$foreground = [ordered]@{ mdpi = 108; hdpi = 162; xhdpi = 216; xxhdpi = 324; xxxhdpi = 432 }
foreach ($d in $foreground.Keys) {
    $size = $foreground[$d]
    $pair = New-Canvas $size
    $bmp = $pair[0]; $g = $pair[1]
    $g.Clear($bg)
    Draw-Artwork $g $size
    $g.Dispose()
    Save-Png $bmp "$ResDir\mipmap-$d\ic_launcher_foreground.png"
    $bmp.Dispose()
}

$src.Dispose()
Write-Output "icons written to $ResDir"

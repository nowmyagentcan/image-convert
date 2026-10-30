# Writes the EMF+ fixtures with GDI+ (System.Drawing); run in Windows PowerShell from this folder.
# Each has a 1 x 0.5 inch frame (96 x 48 px at 96 DPI), a transparent background, a steel-blue
# rectangle and an orange circle on it, as EMF+ only (emf-plus.emf) and EMF+ with GDI records
# (emf-dual.emf). GDI+ places the drawing in the frame by the recording display's scaling, so the
# tests check colours and anti-aliasing here; tests/common writes plain EMF and WMF with exact
# geometry.
Add-Type -AssemblyName System.Drawing
# DPI-aware, so the reference context's resolution and its size in millimetres agree.
Add-Type -Namespace Win32 -Name User32 -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();'
[void][Win32.User32]::SetProcessDPIAware()
$reference = [Drawing.Graphics]::FromHwnd([IntPtr]::Zero)
$hdc = $reference.GetHdc()
$frame = New-Object Drawing.RectangleF 0, 0, 1, 0.5
foreach ($pair in @(('plus', 'EmfPlusOnly'), ('dual', 'EmfPlusDual'))) {
    $path = Join-Path $PWD "emf-$($pair[0]).emf"
    $metafile = New-Object Drawing.Imaging.Metafile($path, $hdc, $frame, [Drawing.Imaging.MetafileFrameUnit]::Inch, [Drawing.Imaging.EmfType]$pair[1])
    $g = [Drawing.Graphics]::FromImage($metafile)
    $g.PageUnit = [Drawing.GraphicsUnit]::Inch
    $g.PageScale = 1 / 96
    $g.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.FillRectangle([Drawing.Brushes]::SteelBlue, 8, 8, 80, 32)
    $g.FillEllipse([Drawing.Brushes]::Orange, 60, 12, 24, 24)
    $g.Dispose()
    $metafile.Dispose()
}
$reference.ReleaseHdc($hdc)
$reference.Dispose()

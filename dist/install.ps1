# runtime-orbit installer — Windows (PowerShell).
#
#   irm https://slothlabs.org/install/runtime-orbit.ps1 | iex
#   irm https://raw.githubusercontent.com/slothlabsorg/runtime-orbit/main/dist/install.ps1 | iex
#
# Downloads the latest release binary and installs runtime-orbit.exe to
# %LOCALAPPDATA%\runtime-orbit\bin, adding it to your user PATH, plus
# r-orbit.cmd and orbit.cmd shortcuts.

$ErrorActionPreference = "Stop"
$Repo = "slothlabsorg/runtime-orbit"
$Version = if ($env:ORBIT_VERSION) { $env:ORBIT_VERSION } else { "latest" }

$arch = (Get-CimInstance Win32_Processor).Architecture
# 9 = x64, 12 = ARM64
$target = if ($arch -eq 12) { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" }
$asset  = "runtime-orbit-$target.zip"

if ($Version -eq "latest") {
  $url = "https://github.com/$Repo/releases/latest/download/$asset"
} else {
  $url = "https://github.com/$Repo/releases/download/$Version/$asset"
}

$dir = Join-Path $env:LOCALAPPDATA "runtime-orbit\bin"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$tmp = Join-Path $env:TEMP $asset

Write-Host "> Downloading $asset..." -ForegroundColor Cyan
Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
Expand-Archive -Path $tmp -DestinationPath $dir -Force
Remove-Item $tmp -Force

# Short aliases. Windows has no reliable unprivileged symlink, so use shims that
# forward every argument and preserve the exit code.
foreach ($alias in @("r-orbit", "orbit")) {
  $shim = Join-Path $dir "$alias.cmd"
  Set-Content -Path $shim -Encoding ASCII -Value @(
    '@echo off',
    '"%~dp0runtime-orbit.exe" %*'
  )
  Write-Host "OK linked $alias -> runtime-orbit" -ForegroundColor Green
}

# Add to the user PATH if not already there.
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$dir*") {
  [Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
  Write-Host "! Added $dir to your PATH — open a new terminal to pick it up." -ForegroundColor Yellow
}

Write-Host "OK installed runtime-orbit to $dir\runtime-orbit.exe" -ForegroundColor Green
Write-Host ""
Write-Host "Next — on the machine that needs the RAM:" -ForegroundColor Green
Write-Host "      runtime-orbit setup --ip <donor-ip>"
Write-Host ""
Write-Host "   ...and on the machine lending its runtime:"
Write-Host "      runtime-orbit donor setup"

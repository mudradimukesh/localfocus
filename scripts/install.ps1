$ErrorActionPreference = "Stop"

$RootDir = Resolve-Path (Join-Path $PSScriptRoot "..")
$InstallDir = if ($env:LOCAL_FOCUS_INSTALL_DIR) {
    $env:LOCAL_FOCUS_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "Programs\LocalFocus"
}

Push-Location $RootDir
cargo build --release
Pop-Location

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item -Force (Join-Path $RootDir "target\release\local-focus.exe") (Join-Path $InstallDir "local-focus.exe")

Write-Host "Installed local-focus to: $InstallDir\local-focus.exe"
Write-Host "Start from PowerShell: `"$InstallDir\local-focus.exe`" serve"
Write-Host "Dashboard: http://127.0.0.1:4799"

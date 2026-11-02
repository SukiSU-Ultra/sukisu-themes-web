$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$env:CC_aarch64_unknown_linux_musl = Join-Path $PSScriptRoot "zigcc-aarch64-musl.bat"
$env:AR_aarch64_unknown_linux_musl = Join-Path $PSScriptRoot "ar-aarch64-musl.bat"
Push-Location $root
try {
    cargo build --release --target aarch64-unknown-linux-musl
} finally {
    Pop-Location
}

# Build/run this project from a fresh shell.
# Usage:  powershell -ExecutionPolicy Bypass -File build.ps1
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

$env:RUSTUP_HOME = "$root\.toolchain\rustup"
$env:CARGO_HOME  = "$root\.toolchain\cargo"
$env:Path = "$root\.toolchain\cargo\bin;$root\.toolchain\w64devkit\bin;$env:Path"

cargo build
exit $LASTEXITCODE
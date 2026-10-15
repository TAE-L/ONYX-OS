# Build the workspace with the portable toolchain; tee FULL output to a log.
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$env:RUSTUP_HOME = "$root\.toolchain\rustup"
$env:CARGO_HOME  = "$root\.toolchain\cargo"
$env:Path = "$root\.toolchain\cargo\bin;$root\.toolchain\w64devkit\bin;$env:Path"
Set-Location $root
$log = Join-Path $root 'logs\build\build.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
Remove-Item $log -ErrorAction SilentlyContinue
cargo build 2>&1 | ForEach-Object { $_ } | Out-File -Encoding utf8 $log
Write-Host "exit=$LASTEXITCODE log=$log"
Add-Content $log "exit=$LASTEXITCODE"
# Show the tail so a wrapper can see the final state.
Get-Content $log -Tail 25
exit $LASTEXITCODE
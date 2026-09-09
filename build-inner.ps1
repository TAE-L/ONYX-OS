# Build the workspace with the portable toolchain; tee FULL output to a log.
$ErrorActionPreference = 'Continue'
$root = 'c:\Users\ASUS\Desktop\PROJECT OS'
$env:RUSTUP_HOME = "$root\.toolchain\rustup"
$env:CARGO_HOME  = "$root\.toolchain\cargo"
$env:Path = "$root\.toolchain\cargo\bin;$root\.toolchain\w64devkit\bin;$env:Path"
Set-Location $root
$log = Join-Path $env:TEMP 'm9-build.log'
Remove-Item $log -ErrorAction SilentlyContinue
cargo build 2>&1 | ForEach-Object { $_ } | Out-File -Encoding utf8 $log
Write-Host "exit=$LASTEXITCODE log=$log"
Add-Content $log "exit=$LASTEXITCODE"
# Show the tail so a wrapper can see the final state.
Get-Content $log -Tail 25
exit $LASTEXITCODE
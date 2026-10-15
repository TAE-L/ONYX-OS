# Run every QEMU suite SEQUENTIALLY and summarise.
#
# Usage:  powershell -ExecutionPolicy Bypass -File tests\run-all.ps1
#         powershell -ExecutionPolicy Bypass -File tests\run-all.ps1 -Only fs,block
#
# Sequential is not a style preference, it is a correctness requirement: every
# suite starts with `Stop-Process qemu-system-x86_64`, so two suites running at
# once kill each other's VM and produce failures that look exactly like
# regressions. Never run these in parallel.
param([string]$Only = '')

$ErrorActionPreference = 'Continue'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
Set-Location $root

$summary = Join-Path $root 'logs\run-all\summary.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $summary) | Out-Null
Remove-Item $summary -ErrorAction SilentlyContinue

$all = Get-ChildItem $here -Filter 'test-*.ps1' -File | Sort-Object Name
if ($Only) {
    $want = $Only.Split(',') | ForEach-Object { $_.Trim() }
    $all = $all | Where-Object { $n = $_.BaseName -replace '^test-', ''; $want -contains $n }
}

$results = @()
foreach ($t in $all) {
    Write-Host "=== $($t.Name)" -ForegroundColor Cyan
    Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400

    $out = & powershell -ExecutionPolicy Bypass -File $t.FullName 2>&1
    $code = $LASTEXITCODE
    $verdict = if ($code -eq 0) { 'PASS' } else { 'FAIL' }
    $results += [pscustomobject]@{ Suite = $t.BaseName; Verdict = $verdict; Exit = $code }

    # The suite's own verdict line is more trustworthy than the exit code alone.
    $line = $out | Where-Object { $_ -match '^(RESULT|\[.*\] done|PASSED)' } | Select-Object -Last 1
    $colour = if ($verdict -eq 'PASS') { 'Green' } else { 'Red' }
    Write-Host "    $verdict  $line" -ForegroundColor $colour
    "$($t.BaseName)`t$verdict`t$code`t$line" | Add-Content $summary
}

Get-Process qemu-system-x86_64 -ErrorAction SilentlyContinue | Stop-Process -Force

$failed = @($results | Where-Object { $_.Verdict -eq 'FAIL' })
Write-Host ''
Write-Host "$($results.Count) suites, $($results.Count - $failed.Count) passed, $($failed.Count) failed" -ForegroundColor $(if ($failed.Count) { 'Red' } else { 'Green' })
if ($failed.Count) { Write-Host "failed: $($failed.Suite -join ', ')"; exit 1 }
Write-Host "summary: $summary"
exit 0
# M9.8 dev helper: extract the hand-assembled AP trampoline blobs out of
# `kernel/src/smp.rs` into flat binaries so they can be disassembled (objdump)
# and diffed against the documented listing. Not part of the build.
$root = Split-Path -Parent $MyInvocation.MyCommand.Path | Split-Path -Parent
$src = Get-Content (Join-Path $root 'kernel\src\smp.rs') -Raw
$out = Join-Path $root 'target'

function Get-Blob([string]$name, [int]$count) {
    $pattern = 'const ' + $name + ': \[u8; ' + $count + '\] = \[(.*?)\];'
    $m = [regex]::Match($src, $pattern, 'Singleline')
    if (-not $m.Success) { throw "blob $name not found" }
    $body = $m.Groups[1].Value
    # Drop `//` comments: the listing documents patch sites with hex offsets
    # that would otherwise be picked up as bytes.
    $body = ($body -split "`n" | ForEach-Object { ($_ -split '//')[0] }) -join "`n"
    $bytes = [regex]::Matches($body, '0x([0-9A-Fa-f]{2})') |
        ForEach-Object { [Convert]::ToByte($_.Groups[1].Value, 16) }
    if ($bytes.Count -ne $count) { throw "$name has $($bytes.Count) bytes, expected $count" }
    return ,[byte[]]$bytes
}

[IO.File]::WriteAllBytes((Join-Path $out 'code16.bin'), (Get-Blob 'LOW_CODE' 81))
[IO.File]::WriteAllBytes((Join-Path $out 'stub64.bin'), (Get-Blob 'STUB64' 28))
[IO.File]::WriteAllBytes((Join-Path $out 'gdt.bin'), (Get-Blob 'GDT_BLOB' 24))
Write-Host 'wrote target\code16.bin, target\stub64.bin, target\gdt.bin'

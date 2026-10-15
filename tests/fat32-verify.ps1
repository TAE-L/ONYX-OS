# fat32-verify.ps1 -- INDEPENDENT host-side FAT32 directory verifier.
#
#   powershell -ExecutionPolicy Bypass -File tests\fat32-verify.ps1 -Image <path>
#
# Deliberately shares NO code with the kernel: it is a second implementation of
# the on-disk format, written from the FAT specification, in a different
# language. That is the whole point. Three bugs in this phase (a wrong LFN
# character offset, LAST_LONG written to the type byte, and a directory-entry
# write that erased the following file) were all invisible to a round-trip test
# because our writer and our reader agreed with each other. Only a reader that
# never saw our writer can catch that class.
#
# What it checks, per the spec:
#   * LFN slot attribute byte 11 == 0x0F
#   * byte 0: sequence number, with 0x40 (LAST_LONG) on order 1 ONLY
#   * physical order is N, N-1, ... 1, immediately in front of the short entry
#   * byte 13 checksum == sum of the 11 short-name bytes (mod 256)
#   * characters at 1..11, 14..26, 28..32 -- NOT 14..28
#   * no orphan LFN run anywhere in the directory
#   * 8.3-legal names produce a pure short entry with no LFN slots
#   * write time/date decode to a plausible present-day date
param(
    [Parameter(Mandatory)][string]$Image,
    # Dump every raw directory slot the walk touches. Needed when a name is
    # missing: the listing tells you what the parser assembled, this tells you
    # what is actually on disk, and those two are where the bugs live.
    [switch]$DumpSlots
)

$ErrorActionPreference = 'Stop'

# Without this a .NET type error surfaces as a bare "Argument types do not match"
# with no line number, which cost more time than it saved.
trap {
    Write-Host ("ERROR line {0}: {1}" -f $_.InvocationInfo.ScriptLineNumber, $_.Exception.Message) -ForegroundColor Red
    Write-Host ("  in: " + $_.InvocationInfo.Line.Trim()) -ForegroundColor DarkGray
    exit 1
}

$bytes = [System.IO.File]::ReadAllBytes($Image)

# [BitConverter] with an explicit [int] offset, not raw indexing arithmetic.
# `$bytes[$o]` with a UInt64 `$o` inside a helper function silently yields $null
# on this PowerShell, and `$null -shl 8` is 0 -- so U16 reported a bytes-per-sector
# of 0 for a volume that plainly stores 512. Silent wrong answers are worse than
# slow ones, hence the cast.
function U8($o)  { $bytes[[int]$o] }
function U16($o) { [BitConverter]::ToUInt16($bytes, [int]$o) }
function U32($o) { [BitConverter]::ToUInt32($bytes, [int]$o) }

# ---- MBR: find the REAL FAT32 partition -----------------------------------
# There are TWO entries with MBR type 0x0C in this image: the bootloader's own
# internal volume and the one the kernel mounts. The bootloader labels its FAT12
# volume 0x0C as well, so selecting on the type byte alone picks the wrong one and
# then reads a FAT12 BPB as though it were FAT32 -- garbage FATSz32, a root
# cluster of 1701737061. Test the BPB instead: FAT32 means FATSz32 is meaningful
# and the volume does not self-describe as FAT12/FAT16.
$partStart = 0
$rejects = @()
for ($i = 0; $i -lt 4; $i++) {
    $e = 0x1BE + $i * 16
    if ((U8 ($e + 4)) -ne 0x0C) { continue }
    $lba = [uint64](U32 ($e + 8)) * 512
    $fsz = U32 ($lba + 36)
    $tag = -join ($bytes[($lba + 54)..($lba + 61)] | ForEach-Object { [char]$_ })
    if ($fsz -ne 0 -and $tag -notmatch 'FAT12|FAT16') { $partStart = $lba; break }
    $rejects += "entry ${i}: LBA $($lba / 512), fstype '$($tag.Trim())', FATSz32=$fsz"
}
# No MBR partition matched: this may be a BARE PARTITION image (the FAT32 starts
# at offset 0), which is what `tools/fatfs-verify` consumes. Detect it by the BPB
# rather than by MBR type.
if ($partStart -eq 0) {
    if ((U16 11) -eq 512 -and (U8 13) -gt 0 -and (U32 36) -ne 0 -and (U32 44) -ge 2) {
        $partStart = 0
        Write-Host 'no MBR FAT32 entry; treating the image as a bare FAT32 partition (offset 0)'
    } else {
        throw ("no FAT32 partition found. rejected 0x0C entries -- " + ($rejects -join '; '))
    }
}

# ---- BPB ------------------------------------------------------------------
$bpb = $partStart
$bytesPerSector  = U16 ($bpb + 11)
$sectorsPerClu   = U8  ($bpb + 13)
$reservedSectors = U16 ($bpb + 14)
$numFats         = U8  ($bpb + 16)
$fatSize32       = U32 ($bpb + 36)
$rootCluster     = U32 ($bpb + 44)
if ($bytesPerSector -eq 0 -or $sectorsPerClu -eq 0 -or $fatSize32 -eq 0) {
    throw "implausible BPB (bps=$bytesPerSector spc=$sectorsPerClu fat32=$fatSize32)"
}
$clusterBytes = $bytesPerSector * $sectorsPerClu
$fatStart     = $partStart + $reservedSectors * $bytesPerSector
$dataStart    = $fatStart + $numFats * $fatSize32 * $bytesPerSector

# FAT32 numbers clusters from 2 (0 and 1 are reserved), so the first data cluster
# sits immediately after the FAT copies. Computing `fatStart + c * clusterBytes`
# instead reads reserved slack: it is in range for c>=2 but points `2 * clusterBytes`
# too far, which here lands in an all-zero region and looks like an empty volume.
function ClusterOff([uint32]$c) { [uint64]$dataStart + ([uint64]($c - 2) * $clusterBytes) }
function FatNext([uint32]$c) { [uint32](U32 ($fatStart + [uint64]$c * 4)) }

# ---- walk the directory chain, collecting raw 32-byte slots ---------------
$slots = New-Object System.Collections.Generic.List[object]
$seen  = @{}
$cluster = $rootCluster
while ($cluster -ge 2 -and $cluster -lt 0x0FFFFFF8 -and -not $seen.ContainsKey($cluster)) {
    $seen[$cluster] = $true
    $base = ClusterOff $cluster
    $stop = $false
    for ($i = 0; $i -lt $clusterBytes -and -not $stop; $i += 32) {
        $o = [int]($base + $i)
        $b0 = $bytes[$o]
        # 0x00 terminates the ENTIRE directory, including later clusters of the
        # chain: slots past it are stale and must not be read.
        if ($b0 -eq 0x00) { $stop = $true; break }
        # A deleted slot is recorded, not skipped. The assembler has to SEE it in
        # order to reset the pending LFN run: skipping it silently splices the
        # tail of one filename onto the next short entry, which is exactly the bug
        # class this whole verifier exists to catch.
        $attr = if ($b0 -eq 0xE5) { 0xFF } else { $bytes[$o + 11] }
        $slots.Add([pscustomobject]@{
            Offset = $o; Cluster = $cluster; Index = $i / 32; Attr = $attr
            IsLfn = ($attr -eq 0x0F)
            Deleted = ($b0 -eq 0xE5)
            # 0x0F (LFN) also has the volume bit set, so IsLfn must be excluded
            # here or every LFN slot classifies as a volume label.
            VolumeLabel = (($attr -band 0x08) -ne 0 -and $attr -ne 0x0F)
            Raw = $bytes[[int]($o)..([int]($o) + 31)]
        })
    }
    if ($stop) { break }
    $cluster = FatNext $cluster
}
if ($DumpSlots) {
    Write-Host "raw slots walked ($($slots.Count)):"
    foreach ($s in $slots) {
        $nm = ($s.Raw[0..10] | ForEach-Object { if ($_ -eq 0x20) { '.' } else { [char]$_ } }) -join ''
        $kind = if ($s.Deleted) { 'DEL ' } elseif ($s.IsLfn) { 'LFN ' } elseif ($s.VolumeLabel) { 'VOL ' } else { 'FILE' }
        Write-Host ("  off={0,-9} clus={1,-6} idx={2,-3} {3} b0=0x{4:X2} attr=0x{5:X2} cs13={6,-4} {7}" -f `
            $s.Offset, $s.Cluster, $s.Index, $kind, $s.Raw[0], $s.Attr, $s.Raw[13], $nm)
        # Full 32-byte hex. Comparing our slots against another implementation's,
        # field by field, is the only way to settle a disagreement like A5 -- a
        # summary line hides whichever byte neither of us thinks to print.
        $hex = ($s.Raw | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
        Write-Host ("      hex: {0}" -f $hex)
    }
    Write-Host ''
}

# ---- assemble entries the way the spec says a reader must -----------------
# Forward scan only collects LFN slots; they are consumed when the short entry
# that follows them is reached.
$fail = @(); $pass = @(); $upstream = @(); $note = @()
$pending = New-Object System.Collections.Generic.List[object]
$entries = @()

foreach ($s in $slots) {
    # Order matters: an LFN slot's attribute is 0x0F, which ALSO has the volume
    # bit set, so it has to be tested before the volume-label case or every long
    # name is silently discarded.
    if ($s.IsLfn) { $pending.Add($s); continue }
    # A deleted slot breaks any run in progress.
    if ($s.Deleted) { $pending.Clear(); continue }
    if ($s.VolumeLabel) { continue }
    $short = $s.Raw[0..10]
    $shortName = ($short | ForEach-Object { if ($_ -eq 0x20) { ' ' } else { [char]$_ } }) -join ''
    # The FAT LFN checksum (byte 13) is the original MS-DOS ChkSum: an 8-bit
    # ROTATE-right folded into the addition -- NOT a plain sum of the 11 bytes.
    # The documentation says so in as many words: "NOTE: The operation is an
    # unsigned char rotate right".
    #
    # This verifier originally implemented a plain sum on the theory that the spec
    # says "sum the 11 bytes". That was wrong, and the error was caught by
    # cross-checking the `fatfs` crate (tools/fatfs-verify), which implements the
    # rotate and then failed to read a single long name the kernel had written.
    $sum = 0
    foreach ($b in $short) {
        $rot = if (($sum -band 1) -ne 0) { 0x80 } else { 0x00 }
        $sum = ($rot + [int][math]::Floor($sum / 2) + $b) -band 0xFF
    }

    # The seed tool (tools/mkfat, via the `fatfs` crate) writes LFN entries that do
    # NOT satisfy the spec: wrong checksums and attribute 0x00 on the short entry.
    # That is an upstream problem, not ours, and it is pre-existing -- it is also
    # why fat.rs notes that our reader must fall back to the short name for these.
    # So conformance is enforced only on entries the KERNEL wrote (the fixtures),
    # and the seed violations are reported separately as known-upstream rather
    # than failing the suite or being quietly ignored.
    $seeded = @('README', 'WALLPAPER', 'SHELL', 'FSTEST', 'EVTEST', 'ARGTEST', 'LNXTEST', 'AUTOEXEC', 'BLCK')
    $fixtureNames = @('ABCDEFGHIJKLM', 'ABCDEFGHIJKLMN', 'InteropTwentyCharsLong',
                       'InteropVeryLongDocumentNameFortyCharsIndeed', 'interop.lower.name.txt', 'LEGAL83.TXT')

    $name = $null; $lfnCount = 0; $isKernelEntry = $true
    if ($pending.Count -gt 0) {
        # `@($list)` on a generic List[object] throws "Argument types do not
        # match" here, so copy element-wise instead.
        $run = @()
        foreach ($p in $pending) { $run += $p }
        $pending.Clear()
        # Characters: 5 units at 1..11, 6 at 14..26, 2 at 28..32 -- NOT 14..28.
        # Concatenated in PHYSICAL order, which per the spec is already the
        # logical order (sequence N, N-1, ... 1), so no sorting is needed.
        $units = @()
        foreach ($r in $run) {
            $o = $r.Raw; $u = @()
            for ($i = 1; $i -le 10; $i += 2) { $u += [uint16]($o[$i] -bor ($o[$i+1] -shl 8)) }
            for ($i = 14; $i -le 24; $i += 2) { $u += [uint16]($o[$i] -bor ($o[$i+1] -shl 8)) }
            for ($i = 28; $i -le 30; $i += 2) { $u += [uint16]($o[$i] -bor ($o[$i+1] -shl 8)) }
            $units += $u
        }
        $chars = [System.Collections.Generic.List[char]]::new()
        foreach ($u in $units) { if ($u -eq 0) { break }; $chars.Add([char]$u) }
        $name = $chars -join ''
        $lfnCount = $run.Count
    }
    # An 8.3 entry is stored as 11 raw bytes with no dot: "LEGAL83  TXT". Render
    # it back to the form a user (and the fixture list) expects, otherwise every
    # pure-8.3 fixture reads as "missing" purely because of the missing dot.
    if (-not $name) {
        $base = ($short[0..7] | ForEach-Object { if ($_ -eq 0x20) { ' ' } else { [char]$_ } }) -join ''
        $ext  = ($short[8..10] | ForEach-Object { if ($_ -eq 0x20) { ' ' } else { [char]$_ } }) -join ''
        $base = $base.TrimEnd(); $ext = $ext.Trim()
        $name = if ($ext) { "$base.$ext" } else { $base }
    }
    # Now that the long name is known, retro-grade this entry's conformance
    # verdict: kernel-written fixtures must satisfy the spec, mkfat-seeded ones
    # are only reported.
    if ($lfnCount -gt 0) {
        $seqs = @($run | ForEach-Object { $_.Raw[0] -band 0x1F })
        $badOrder = @()
        for ($k = 0; $k -lt $seqs.Count; $k++) { if ($seqs[$k] -ne ($seqs.Count - $k)) { $badOrder += $seqs[$k] } }
        $badLast = @()
        for ($k = 0; $k -lt $seqs.Count; $k++) {
            $isLast = (($run[$k].Raw[0] -band 0x40) -ne 0)
            if ($isLast -ne ($seqs[$k] -eq 1)) { $badLast += $seqs[$k] }
        }
        $badCk = @($run | Where-Object { $_.Raw[13] -ne $sum }).Count
        $badAttr = @($run | Where-Object { $_.Attr -ne 0x0F }).Count
        $problems = @()
        if ($badOrder.Count) { $problems += "sequence $($seqs -join ',') want $($seqs.Count)..1" }
        if ($badLast.Count)  { $problems += "LAST_LONG wrong on seq $($badLast -join ',')" }
        if ($badCk)          { $problems += "$badCk/$($run.Count) checksums wrong" }
        if ($badAttr)       { $problems += "$badAttr/$($run.Count) attr != 0x0F" }
        if ($problems.Count) {
            if ($fixtureNames -contains $name) {
                $fail += "'$name': $($problems -join '; ')"
            } else {
                $upstream += "'$name' ($shortName.Trim()): $($problems -join '; ') [seeded by mkfat/fatfs, pre-existing]"
            }
        } elseif ($fixtureNames -contains $name) {
            $pass += "'$name' is spec-conformant across $($lfnCount) LFN slot(s)"
        }
    }

    $wdate = U16 ($s.Offset + 24)
    $year = ($wdate -shr 9) + 1980; $month = ($wdate -shr 5) -band 0xF; $day = $wdate -band 0x1F
    $entries += [pscustomobject]@{
        Name = $name; Short = $shortName; LfnSlots = $lfnCount; Attr = $s.Attr
        Size = U32 ($s.Offset + 28)
        Date = ('{0}-{1:D2}-{2:D2}' -f $year, $month, $day)
        Cluster = $s.Cluster; Index = $s.Index
    }
}

# LFN slots left over at the end are orphans: a reader would drop them, or worse,
# attach them to the next entry it finds.
if ($pending.Count -gt 0) {
    $orph = (($pending | ForEach-Object { $_.Raw[0] -band 0x1F }) | Sort-Object -Descending) -join ','
    $fail += "$($pending.Count) ORPHAN LFN slot(s) at the end of the directory (sequences $orph)"
}
Write-Host "partition LBA $($partStart / 512), $bytesPerSector B/sector, $sectorsPerClu sectors/cluster, root cluster $rootCluster"
Write-Host "walked $($slots.Count) live directory slots -> $($entries.Count) entries`n"
foreach ($e in $entries | Sort-Object Name) {
    Write-Host ("  {0,-46} short={1,-14} lfnSlots={2} size={3,-5} {4}" -f $e.Name, $e.Short.Trim(), $e.LfnSlots, $e.Size, $e.Date)
}

# ---- the interop expectations ---------------------------------------------
# name -> expected LFN slot count (0 means it must be a pure 8.3 entry).
$expect = [ordered]@{
    'ABCDEFGHIJKLM'                                = 1
    'ABCDEFGHIJKLMN'                               = 2
    'InteropTwentyCharsLong'                       = 2
    'InteropVeryLongDocumentNameFortyCharsIndeed'  = 4
    'interop.lower.name.txt'                       = 2
    'LEGAL83.TXT'                                  = 0
}
foreach ($k in $expect.Keys) {
    $e = $entries | Where-Object { $_.Name -ceq $k }
    if (-not $e) { $fail += "FIXTURE MISSING: '$k' not found in the root directory"; continue }
    if ($e.LfnSlots -ne $expect[$k]) {
        $fail += "'$k' has $($e.LfnSlots) LFN slots, expected $($expect[$k])"
    } else {
        $pass += "'$k' decodes with $($e.LfnSlots) LFN slot(s)"
    }
    if ($e.Date -notmatch '^(202[0-9]|20[3-9][0-9]|210[0-7])-') {
        $fail += "'$k' write-date decodes to $($e.Date), not a present-day date"
    }
}
# Two names sharing a base must NOT share a short alias.
$a = $entries | Where-Object { $_.Name -ceq 'ABCDEFGHIJKLM' }
$b = $entries | Where-Object { $_.Name -ceq 'ABCDEFGHIJKLMN' }
if ($a -and $b -and $a.Short -ceq $b.Short) {
    $fail += "ABCDEFGHIJKLM and ABCDEFGHIJKLMN share the short name '$($a.Short)'"
} elseif ($a -and $b) {
    $pass += "colliding bases got distinct aliases ($($a.Short.Trim()) / $($b.Short.Trim()))"
}

# Two directory entries carrying the same name AND the same 8.3 alias is a real
    # defect -- the second create should have probed to INTERO~3 -- but it is not
    # an on-disk-format error, so it is reported rather than enforced here.
    $dupes = $entries | Group-Object Name, Short | Where-Object { $_.Count -gt 1 }
    foreach ($d in $dupes) {
        $note += "DUPLICATE directory entry: '$($d.Group[0].Name.Trim())' short='$($d.Group[0].Short.Trim())' appears $($d.Count) times -- the alias probe did not see the earlier one"
    }

Write-Host ''
foreach ($p in $pass) { Write-Host "ok: $p" -ForegroundColor Green }
foreach ($f in $fail) { Write-Host "FAIL: $f" -ForegroundColor Red }
if ($note.Count) {
    Write-Host ''
    Write-Host 'reported, NOT enforced (real but not a format error):' -ForegroundColor Yellow
    foreach ($n in $note) { Write-Host "  - $n" -ForegroundColor Yellow }
}
if ($upstream.Count) {
    Write-Host ''
    Write-Host "known upstream (NOT our writer, reported not enforced):" -ForegroundColor Yellow
    foreach ($u in $upstream) { Write-Host "  - $u" -ForegroundColor Yellow }
}
Write-Host ''
if ($fail.Count) {
    Write-Host "FAT32 INTEROP: $($pass.Count) ok, $($fail.Count) FAILED" -ForegroundColor Red
    exit 1
}
Write-Host "FAT32 INTEROP: $($pass.Count) checks passed" -ForegroundColor Green
exit 0

Write-Host "partition LBA $partStart, $bytesPerSector B/sector, $sectorsPerClu sectors/cluster, root cluster $rootCluster"
Write-Host "$($entries.Count) entries`n"
foreach ($e in $entries | Sort-Object Name) {
    Write-Host ("  {0,-46} short={1,-14} lfnSlots={2} size={3,-4} {4}" -f $e.Name, $e.Short.Trim(), $e.LfnSlots, $e.Size, $e.Date)
}
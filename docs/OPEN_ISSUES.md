# Open issues — flagged, NOT fixed, for a future session

Written 2026-10-02; entries A6 onward added 2026-10-15. Everything here is
something I found, judged out of scope for
the current task, and left alone **on purpose**. Nothing in this list is
accidentally skipped: each entry says what it is, why it was left, what it costs,
and where to start.

Read this before re-running the phase list in `docs/PLAN.md` — several entries
there are "done" in the sense that the code exists, not that the behaviour is
finished.

---

## A. Bugs found and deliberately left unfixed

### A1. Duplicate directory entry with a colliding 8.3 alias — FAT
**Found by:** `tests/test-fatinterop.ps1` (reports it, does not fail on it).
**Symptom:** `InteropVeryLongDocumentNameFortyCharsIndeed` exists **twice** in the
root, both under alias `INTERO~2`, one with size 0 and one with size 2. The alias
probe should have produced `INTERO~3` for the second create.
**Why left:** both entries are spec-conformant on disk, so this is not an interop
bug. Isolating it needs shell tracing or a `create_entry` call counter — a
separate debugging session.
**Suspects, in order:** (a) the guest shell's `write` calls `open(O_CREAT)` twice;
(b) `create_entry` is reached twice for one path; (c) `derive_name`'s probe scans
a directory snapshot taken before the first entry became visible.
**Cost:** a file can silently exist twice; two writes may land on different inodes.
**Start at:** `kernel/src/fat.rs` `derive_name`, and `user/shell` `write_file`.

### A2. `mkfat` / the `fatfs` crate writes non-conformant LFN entries — upstream
**Found by:** `tests/fat32-verify.ps1`, reported under "known upstream".
**Symptom:** every seeded file (`README.TXT`, `SHELL.ELF`, `AUTOEXEC.TXT`, …) has a
wrong LFN checksum, and its short entry has attribute `0x00` instead of `0x20`.
`fat.rs` already notes this tool misplacing the LFN terminator for names >= 12.
**Why left:** not our code, and our reader already tolerates it by falling back to
the short name. Fixing means replacing the seed tool, not the kernel.
**Cost:** every seeded file relies on the short-name fallback to be visible at
all. If that fallback ever regresses, the whole boot volume disappears.
**Start at:** `tools/mkfat`, or stop using it and seed with a conforming writer.

### A3. Windows refuses to mount the produced image
**Found by:** the best-effort `Mount-DiskImage` step in `test-fatinterop.ps1`.
**Symptom:** it reports "corrupted and unreadable" on the raw image, so the *real
Windows* verdict on our long names is still unproven. The independent parser
passes, which is strong but is not Windows.
**Why left:** mounting a raw `.img` needs elevation and a permissive host; the
failure may also concern the hybrid MBR rather than the FAT32 volume the parser
validated.
**Cost:** "interop verified" currently means "a second implementation agrees", not
"Windows agrees". Treat it as exactly that.
**Start at:** the `Mount-DiskImage` block in `test-fatinterop.ps1`; try a VHD
wrapper around just the FAT32 partition.

### A4. ext2 is not covered by the independent verifier at all
**Found by:** absence, while building the FAT32 verifier.
**Symptom:** 1.3 added ext2 timestamps (`i_atime`/`i_ctime`/`i_mtime` at offsets
8/12/16) with only a kernel round-trip test behind them.
**Why left:** FAT was the urgent one; ext2 interop is a bigger build.
**Cost:** the same bug class now fixed twice in `fat.rs` could still be present in
`ext2.rs`.
**Start at:** a `tests/ext2-verify.ps1` mirroring `fat32-verify.ps1`.

### A5. LFN deletion only reclaims slots inside one cluster
**Found by:** audit of `remove_dir_slots`, pre-existing.
**Symptom:** the backward walk stops at the cluster boundary. A 255-char name
spans 20 slots and can straddle two, so the leading slots leak.
**Why left:** 1.2.1 was already a large change and this needs its own test.
**Cost:** a slow slot leak in directories holding very long names.
**Start at:** `remove_dir_slots` — walk the chain backwards, not just the cluster.

// === A5. fatfs agrees on single-slot long names, disagrees on multi-slot ===
**Found by:** `tools/fatfs-verify` (the third-party `fatfs` oracle), 2026-10-04.
**Symptom:** the oracle reads every long name the kernel writes that needs **one**
**LFN slot**, but returns only the 8.3 alias for any name needing **two or more**.
It reads back its *own* multi-slot names fine (proved by its `--selftest` mode),
so this is a real disagreement, not a broken oracle.
**Status: UNRESOLVED. Not fixed, and deliberately not worked around.**
**What is already ruled out:**
- *Checksum.* The kernel now uses the spec's rotating `ChkSum`, which is what
  `fatfs`, the Linux kernel and the MS-DOS reference all implement. An
  intermediate commit replaced it with a plain sum; that was **wrong** and was
  reverted (see the story in E below).
- *Chunk order.* Sequence *N* holds characters `[(N-1)*13 .. N*13)`. Confirmed
  against a real on-disk dump in the FAT documentation, where the first slot
  (sequence 2) holds the first characters.
- *Run layout.* Both images pass `tests/fat32-verify.ps1` identically: same
  sequence order, same `LAST_LONG` placement, same attributes, same slot counts.
**Therefore:** some byte in the LFN slots still differs from what `fatfs` expects
and the two independent checks do not currently overlap on it. Byte-level
comparison of the two images did not complete.
**Cost:** long names over 13 characters may still be unreadable by some readers.
We have no confirmation that Windows reads them.
**Start at:** dump the 32 raw bytes of a 2-slot run from both an image `fatfs`
wrote and one we wrote, and diff them field by field. `fat32-verify.ps1
-DumpSlots` gets you most of the way; it needs extending to print bytes 12 and
26..28.

### A2. The verifier's first checksum implementation was wrong — reverted
**Found by:** the `fatfs` oracle in `tools/fatfs-verify`.
**Symptom:** the FAT LFN checksum was "fixed" from the spec's rotating `ChkSum`
(`((sum & 1) ? 0x80 : 0) + (sum >> 1) + byte`) to a plain `sum & 0xFF`, on the
reasoning that the spec says "sum the 11 bytes". The reference FAT documentation
says otherwise, in as many words: *"NOTE: The operation is an unsigned char
rotate right"*. The Linux kernel and the `fatfs` crate both implement the rotate.
**Status: reverted.** The kernel is back on the rotate, and the verifier was
corrected to match.
**Also corrected:** `docs/PLAN.md` claimed the checksum had been a shipped bug.
That claim was false and has been rewritten. Keep this in mind when reading it —
it was written from a genuine-looking mismatch that turned out to be the *verifier*
being wrong, not the kernel.
**Cost if it had shipped:** total interop failure on every long filename.

### A6. `fsync` is durable, not crash-consistent — and the proof is weaker than it looks
**Shipped in:** 1.4 (`SYS_FSYNC`/`SYS_FDATASYNC` → `vfs::fsync` → `Fs::fsync`).
**What it does:** pushes a file's metadata out to the device. Verified from
outside the guest by `tests/test-fsync.ps1`, which boots without `-snapshot`,
kills QEMU, and reads the raw image back with the independent PowerShell
reader.
**What it does NOT do — three separate limits, all easy to mistake for done:**
1. **It is not a journal.** An unjournaled ext2 whose metadata update is
   interrupted mid-write still corrupts the volume. `fsync` narrows the window;
   it does not close it. Only 2.4 (ext4 + `jbd2`) does.
2. **The test's "crash" is a QEMU process kill.** QEMU writes through to the
   image file, so this proves the kernel's cache was flushed to the device. It
   says nothing about a real power cut on real hardware, where drive write
   caches lie. Do not read a green `test-fsync` as evidence about physical
   media.
3. **`fdatasync` is not actually distinct** — it shares `fsync`'s body, because
   there is no separate metadata cache to skip. Correct today; it becomes wrong
   the moment a filesystem gains journal replay, at which point the
   distinction must be rebuilt for real.
**Where the two bugs in 1.4's own code both lived:** the guest test called a
function that did not exist (`sayln` vs `say`), and the host-side check
hardcoded `size=40` against a 42-byte payload — so the suite failed against a
kernel that had written the *correct* size. The size is now derived from the
payload literal. Neither was a kernel bug; both would have blocked a clean
commit if not caught.
**Start at:** `kernel/src/vfs.rs::fsync`, `kernel/src/syscall.rs::sys_fsync`,
`tests/test-fsync.ps1`.

---

## B. Design gaps, recorded so they are not mistaken for oversights

| # | Gap | Why it matters |
|---|---|---|
| B1 | **Directory mtime never advances.** Adding or removing a file does not touch the parent's timestamp. | Any "what changed" scan, and a real `ls -lt`, is wrong on both filesystems. |
| B2 | **No `utimensat`/`futimens`.** Timestamps are kernel-driven only. | A user cannot correct a wrong clock or restore an archive's dates. |
| B3 | **ext2 atime only moves on write, not on read.** | Inconsistent with the stated atime contract; the usual fix is once-daily updates, also not implemented. |
| B4 | **Rename moves the slot, not the clusters, and is not atomic.** Same-directory only. | A crash between writing the new run and freeing the old leaves the file under two names; any cached `RawEntry` is stale across a rename. |
| B5 | **FAT is 2-second granular and has no real ctime.** | Creation doubles as ctime and rename cannot be recorded. Format limits, not bugs — but they bound what can be promised. |
## C. Carried over from earlier phases, still true

- **No `fsync`/`fdatasync`.** Nothing is durable; with unjournaled ext2 a power
  cut is a corruption path. This is Phase 1.4, the next planned item.
- **ATA is PIO.** LBA48 (1.1) added address range, not throughput.
- **No per-process address spaces, no networking, no KPTI, no `mmap` family,
  disabled scheduler migration, no modern block driver.** These dominate Phase 1
  in terms of what the OS can actually do.
- **0.4 blit fast path reverted** — `copy_nonoverlapping` on the same-format path
  failed the ramp test with 31/32 wrong pixels. Volatile mapped-memory semantics
  and overlap guarantees are unresolved.
- **0.6 `static mut` cleanup not shipped** — `SyncUnsafeCell::get_mut` needs
  `&mut self` on this toolchain; `surface.rs` still uses a lock-guarded
  `static mut TABLE`.
- **0.3's bulk row writes are unbenchmarked.** The win is assumed, not measured.

---

## D. Things worth knowing about the tooling itself

- **Run suites sequentially.** Every `test-*.ps1` begins with
  `Stop-Process qemu-system-x86_64`; two at once kill each other's VM and produce
  failures indistinguishable from regressions. `tests/run-all.ps1` enforces this.
- **`tests/test-fatinterop.ps1` is the only suite without `-snapshot`.** It must
  never share an image path with another suite, or one run's writes become the
  next run's starting state.
- **`$root` in `tests/*.ps1` is the parent of `tests/`.** It is derived from each
  script's own path, so a one-level move silently points the suite at a
  non-existent `target\debug\images` and fails for an unrelated reason.
- **`build.rs` only rebuilds the image for `build.rs` and `user/` changes.**
  Editing kernel source alone will not refresh `bios.img`'s seeds; touch
  `build.rs` to force it.
- **PowerShell helper functions need `[int]` offsets.** `$bytes[$o]` with a
  `UInt64` index silently yields `$null` inside a function on this host, and
  arithmetic on `$null` returns 0 rather than throwing. `fat32-verify.ps1` uses
  `[BitConverter]` with an explicit `[int]` cast for exactly this reason.

---

## E. The lesson worth carrying forward

Every serious filesystem bug in this phase shared one shape: **our writer and our
reader agreed with each other while both disagreed with the format.**

- wrong LFN character offsets — invisible, we read our own bytes back
- `LAST_LONG` in the type byte — invisible, our reader ignored that bit
- a directory-slot write that erased the next entry — invisible, and it was
  *destroying files*
- a rotating "checksum" that was not the checksum — invisible, 9/9 self-tests green
- LFN chunks stored in reverse — invisible, our reader sorted to match

None of the first four could ever be found by a round-trip test. Two were caught
only by re-reading the spec against a second implementation; the last only by
writing a reader that had never seen our writer.

**And then the verifier itself was wrong.** Its first checksum implementation used
a plain sum where the format uses a rotate — so it "found" a bug in the kernel
that did not exist, and the fix would have broken every long filename on real
hardware. It was caught only because a third implementation (`fatfs`) disagreed
with *both*. That is the same lesson again, one level up: an independent checker
is only as good as its own correctness, and "my new test fails" is not evidence
that the code is wrong.

If this project ever risks cutting verification down to save time, that is the
exact thing to protect.
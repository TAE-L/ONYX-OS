//! M8 ring-3 test: directories + files in subdirectories. Creates /SUBDIR
//! and /SUBDIR/DEEP on the kernel's FAT32 partition, writes files inside
//! them, reads them back and verifies — all through syscalls.
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_WRITE: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_OPEN: u64 = 2;
const SYS_READ: u64 = 3;
const SYS_CLOSE: u64 = 4;
const SYS_LS: u64 = 5;
const SYS_MKDIR: u64 = 6;
const SYS_GETTIME: u64 = 8;
// C2 errno regression also exercises spawn/kill/waitpid.
const SYS_SPAWN: u64 = 7;
const SYS_WAITPID: u64 = 14;
const SYS_KILL: u64 = 15;
// C3: file API completion.
const SYS_STAT: u64 = 18;
const SYS_SEEK: u64 = 19;
const SYS_UNLINK: u64 = 20;
const SYS_RENAME: u64 = 21;
// P16-1.4: fsync / fdatasync. Both take an fd.
const SYS_FSYNC: u64 = 22;
const SYS_FDATASYNC: u64 = 23;

const DIR: &[u8] = b"/SUBDIR";
const PATH: &[u8] = b"/SUBDIR/NOTE.TXT";
const MSG: &[u8] = b"hello from a subdirectory! written by ring 3\n";
const DEEP: &[u8] = b"/SUBDIR/DEEP";
const DEEPFILE: &[u8] = b"/SUBDIR/DEEP/X.TXT";
const DEEPMSG: &[u8] = b"two levels deep\n";
const PASS: &[u8] = b"fstest: PASSED\n";
const FAIL: &[u8] = b"fstest: FAILED\n";
// C3 scratch files (unique to avoid stepping on other suites) + a guaranteed
// missing path used for the -ENOENT errno cases.
const RF: &[u8] = b"/C3REN.DAT";
const RF2: &[u8] = b"/C3REN2.DAT";
const RF3: &[u8] = b"/C3REN3.DAT";
// P16-1.4: durability fixture. `tests/test-fsync.ps1` boots WITHOUT -snapshot,
// so whatever this writes must be visible on the raw image afterwards, read back
// by an INDEPENDENT host-side reader. That is the only real proof that fsync did
// something -- asking the kernel whether it fsynced proves nothing at all.
const FSYNC_FILE: &[u8] = b"/Fsync Durability Probe.txt";
const FSYNC_BODY: &[u8] = b"fsync durability payload 0123456789abcdef\n";
const MP: &[u8] = b"/NO3/C3MISSING.TXT";
const KHEAP: u64 = 0x4444_4444_0000;

// C2: errno normalization helpers — syscalls return -errno on error
// (Linux convention: rax = two's-complement -errno; success is >= 0).
fn is_err(v: u64) -> bool {
    (v as i64) < 0
}
fn err(e: u64) -> u64 {
    (e as i64).wrapping_neg() as u64
}
// Errno values exercised by the regression checks below.
const ENOENT: u64 = 2;
const ESRCH: u64 = 3;
const EBADF: u64 = 9;
const EFAULT: u64 = 14;
const EEXIST: u64 = 17;
const ECHILD: u64 = 10;
const EINVAL: u64 = 22;
const EISDIR: u64 = 21;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[inline(never)]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

#[inline(never)]
unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r8") a4, // `syscall` clobbers rcx/r11, but r8 survives
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

fn say(msg: &[u8]) {
    unsafe {
        let _ = syscall3(SYS_WRITE, 1, msg.as_ptr() as u64, msg.len() as u64);
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe {
        // Root listing first (the kernel prints both mounts).
        let _ = syscall3(SYS_LS, 0, 0, 0);

        // Create the subdirectory. A pre-existing /SUBDIR (replayed image)
        // is tolerated; a real failure shows up when the open below fails.
        if is_err(syscall3(SYS_MKDIR, DIR.as_ptr() as u64, DIR.len() as u64, 0)) {
            say(b"fstest: mkdir /SUBDIR failed (already exists?)\n");
        } else {
            say(b"fstest: mkdir /SUBDIR OK\n");
        }

        // List the (possibly still empty) subdirectory.
        let _ = syscall3(SYS_LS, DIR.as_ptr() as u64, DIR.len() as u64, 0);

        // Create + write a file INSIDE the subdirectory.
        let fd = syscall3(SYS_OPEN, PATH.as_ptr() as u64, PATH.len() as u64, 0);
        if fd == u64::MAX {
            say(b"fstest: open(create) /SUBDIR/NOTE.TXT failed\n");
            syscall3(SYS_EXIT, 1, 0, 0);
        }
        let n = syscall3(SYS_WRITE, fd, MSG.as_ptr() as u64, MSG.len() as u64);
        if n != MSG.len() as u64 {
            say(b"fstest: short write\n");
            syscall3(SYS_EXIT, 1, 0, 0);
        }
        let _ = syscall3(SYS_CLOSE, fd, 0, 0);

        // Reopen + read back + verify.
        let fd2 = syscall3(SYS_OPEN, PATH.as_ptr() as u64, PATH.len() as u64, 0);
        if fd2 == u64::MAX {
            say(b"fstest: reopen failed\n");
            syscall3(SYS_EXIT, 1, 0, 0);
        }
        let mut buf = [0u8; 128];
        let n2 = syscall3(SYS_READ, fd2, buf.as_mut_ptr() as u64, buf.len() as u64);
        let _ = syscall3(SYS_CLOSE, fd2, 0, 0);

        let ok =
            n2 == MSG.len() as u64 && MSG.iter().enumerate().all(|(i, b)| buf[i] == *b);
        if n2 <= buf.len() as u64 {
            say(&buf[..n2 as usize]); // echo what was read back
        }

        // Second level: nested directory + a file inside it.
        let _ = syscall3(SYS_MKDIR, DEEP.as_ptr() as u64, DEEP.len() as u64, 0);
        let fd3 = syscall3(SYS_OPEN, DEEPFILE.as_ptr() as u64, DEEPFILE.len() as u64, 0);
        let ok2 = if fd3 == u64::MAX {
            say(b"fstest: open(create) /SUBDIR/DEEP/X.TXT failed\n");
            false
        } else {
            let w = syscall3(SYS_WRITE, fd3, DEEPMSG.as_ptr() as u64, DEEPMSG.len() as u64);
            let _ = syscall3(SYS_CLOSE, fd3, 0, 0);
            if w == DEEPMSG.len() as u64 {
                let fd4 = syscall3(SYS_OPEN, DEEPFILE.as_ptr() as u64, DEEPFILE.len() as u64, 0);
                if fd4 == u64::MAX {
                    say(b"fstest: reopen /SUBDIR/DEEP/X.TXT failed\n");
                    false
                } else {
                    let mut dbuf = [0u8; 64];
                    let r = syscall3(SYS_READ, fd4, dbuf.as_mut_ptr() as u64, dbuf.len() as u64);
                    let _ = syscall3(SYS_CLOSE, fd4, 0, 0);
                    r == DEEPMSG.len() as u64
                        && DEEPMSG.iter().enumerate().all(|(i, b)| dbuf[i] == *b)
                }
            } else {
                say(b"fstest: short write in /SUBDIR/DEEP\n");
                false
            }
        };

        // Bad-pointer syscalls must be rejected with -1, never fault the
        // kernel (user-pointer validation). Targets: kernel heap, the first
        // byte past the user region, an unmapped user page, and a path
        // pointer into kernel memory.
        let badptr_checks = {
            let kernel_heap = 0x4444_4444_0000u64;
            let past_user_max = 0x1000_0000u64;
            let unmapped_user = 0x0500_0000u64;
            let r1 = syscall3(SYS_READ, 0, kernel_heap, 8);
            let r2 = syscall3(SYS_WRITE, 1, kernel_heap, 8);
            let r3 = syscall3(SYS_READ, 0, past_user_max, 8);
            let r4 = syscall3(SYS_READ, 0, unmapped_user, 8);
            let r5 = syscall3(SYS_OPEN, kernel_heap, 8, 0);
            let ok3 = r1 == err(EFAULT)
                && r2 == err(EFAULT)
                && r3 == err(EFAULT)
                && r4 == err(EFAULT)
                && r5 == err(EFAULT);
            say(if ok3 {
                b"fstest: badptr rejected OK (EFAULT)\n"
            } else {
                b"fstest: badptr ACCEPTED (BUG)\n"
            });
            ok3
        };

        // C2: errno normalization — each failure class returns its specific
        // -errno (not just any negative value).
        // ---------------------------------------------------------------------
        // P16-1.4: fsync / fdatasync.
        // ---------------------------------------------------------------------
        // The file is deliberately NOT removed at the end: `test-fsync.ps1`
        // boots without -snapshot and an independent host-side reader looks for
        // this exact name, size and payload in the raw image after the guest is
        // gone. Deleting it here would leave nothing to verify.
        let fsync_checks = {
            let fd = syscall3(SYS_OPEN, FSYNC_FILE.as_ptr() as u64, FSYNC_FILE.len() as u64, 0);
            if is_err(fd) {
                say(b"fstest: fsync BAD (open)\n");
                false
            } else {
                let w = syscall3(
                    SYS_WRITE,
                    fd,
                    FSYNC_BODY.as_ptr() as u64,
                    FSYNC_BODY.len() as u64,
                );
                let s1 = syscall3(SYS_FSYNC, fd, 0, 0);
                let s2 = syscall3(SYS_FDATASYNC, fd, 0, 0);
                let bad = syscall3(SYS_FSYNC, 99, 0, 0); // not a live fd
                let _ = syscall3(SYS_CLOSE, fd, 0, 0);

                let wrote = !is_err(w) && w == FSYNC_BODY.len() as u64;
                // Re-read it back through a fresh fd: proves the sync did not
                // leave the cache holding something the disk never got.
                let rf = syscall3(SYS_OPEN, FSYNC_FILE.as_ptr() as u64, FSYNC_FILE.len() as u64, 0);
                let mut buf = [0u8; 64];
                let mut n = 0u64;
                if !is_err(rf) {
                    n = syscall3(SYS_READ, rf, buf.as_mut_ptr() as u64, FSYNC_BODY.len() as u64);
                    let _ = syscall3(SYS_CLOSE, rf, 0, 0);
                }
                let mut same = n as usize == FSYNC_BODY.len();
                if same {
                    for i in 0..FSYNC_BODY.len() {
                        if buf[i] != FSYNC_BODY[i] {
                            same = false;
                            break;
                        }
                    }
                }
                let ok = wrote && s1 == 0 && s2 == 0 && bad == err(EBADF) && same;
                say(if ok {
                    b"fstest: fsync OK\n"
                } else {
                    b"fstest: fsync BAD\n"
                });
                ok
            }
        };

        let errno_checks = {
            let missing = b"/NO/SUCH.TXT";
            let op = syscall3(SYS_OPEN, missing.as_ptr() as u64, missing.len() as u64, 0);
            let rd = syscall3(SYS_READ, 99, missing.as_ptr() as u64, 8);
            let cl = syscall3(SYS_CLOSE, 99, 0, 0);
            let mk = syscall3(SYS_MKDIR, DIR.as_ptr() as u64, DIR.len() as u64, 0);
            let sp = syscall3(SYS_SPAWN, missing.as_ptr() as u64, missing.len() as u64, 0);
            let kl = syscall3(SYS_KILL, 9999, 0, 0);
            let wp = syscall3(SYS_WAITPID, 9999, 0, 0);
            let ok5 = op == err(ENOENT)
                && rd == err(EBADF)
                && cl == err(EBADF)
                && mk == err(EEXIST)
                && sp == err(ENOENT)
                && kl == err(ESRCH)
                && wp == err(ECHILD);
            say(if ok5 {
                b"fstest: errno OK\n"
            } else {
                b"fstest: errno BAD\n"
            });
            ok5
        };

        // Monotonic clock smoke test (M9.6-A1): two SYS_GETTIME reads must
        // be non-zero, non-decreasing, and < 10 s apart.
        let t0 = syscall3(SYS_GETTIME, 0, 0, 0);
        let t1 = syscall3(SYS_GETTIME, 0, 0, 0);
        let ok4 =
            t0 != 0 && t0 != u64::MAX && t1 >= t0 && t1 - t0 < 10_000_000_000;
        say(if ok4 {
            b"fstest: time monotonic OK\n"
        } else {
            b"fstest: time monotonic BAD\n"
        });

        // ---------------------------------------------------------------------
        // C3: file API — stat / seek / rename / unlink + their errno cases.
        // ---------------------------------------------------------------------
        let c3_checks = {
            // stat on the file written at the top (size must match, not a dir).
            // P16-1.3: the buffer grew from 16 to 40 bytes -- size, atime, mtime,
            // ctime, flags -- and `is_dir` moved into flags bit 0, so the old
            // `st[8] != 0` test is now reading the low byte of atime.
            let mut st = [0u8; 40];
            let sret = syscall3(
                SYS_STAT,
                PATH.as_ptr() as u64,
                PATH.len() as u64,
                st.as_mut_ptr() as u64,
            );
            let rd64 = |b: &[u8; 40], o: usize| {
                u64::from_le_bytes([
                    b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
                ])
            };
            let st_size = rd64(&st, 0);
            let mut stat_ok =
                sret == 0 && st_size == MSG.len() as u64 && rd64(&st, 32) & 1 == 0;

            // P16-1.3: this file was written moments ago, so all three timestamps
            // must be plausible present-day Unix seconds. The bounds are chosen so
            // that a zeroed stamp (no RTC), a 1970 default, and FAT's 2107 ceiling
            // all fail -- a round-trip test that only checks "non-zero" would pass
            // a driver that wrote garbage.
            const TS_LO: u64 = 1_600_000_000; // 2020-09-13
            const TS_HI: u64 = 4_000_000_000; // 2096, under FAT's 2107 limit
            let ts_ok = [(rd64(&st, 8)), (rd64(&st, 16)), (rd64(&st, 24))]
                .iter()
                .all(|&t| (TS_LO..TS_HI).contains(&t));

            // stat on a directory sets is_dir; missing -> -ENOENT; invalid
            // out-pointer -> -EFAULT.
            let mut ds = [0u8; 40];
            let dsret = syscall3(
                SYS_STAT,
                DIR.as_ptr() as u64,
                DIR.len() as u64,
                ds.as_mut_ptr() as u64,
            );
            let msret = syscall3(
                SYS_STAT,
                MP.as_ptr() as u64,
                MP.len() as u64,
                st.as_mut_ptr() as u64,
            );
            let esret = syscall3(SYS_STAT, DIR.as_ptr() as u64, DIR.len() as u64, KHEAP);
            stat_ok = stat_ok
                && dsret == 0
                && rd64(&ds, 32) & 1 != 0
                && msret == err(ENOENT)
                && esret == err(EFAULT);

            // seek on a fresh open of the file: SET to 0, CUR +3 -> 3,
            // END -1 -> size-1; bad whence -> -EINVAL, bad fd -> -EBADF.
            let sfd = syscall3(SYS_OPEN, PATH.as_ptr() as u64, PATH.len() as u64, 0);
            let n1 = syscall3(SYS_SEEK, sfd, 0, 0);
            let n2 = syscall3(SYS_SEEK, sfd, 3, 1);
            let n3 = syscall3(SYS_SEEK, sfd, u64::MAX, 2); // offset -1
            let nbw = syscall3(SYS_SEEK, sfd, 0, 99);
            let nbf = syscall3(SYS_SEEK, 99, 0, 0);
            let _ = syscall3(SYS_CLOSE, sfd, 0, 0);
            let seek_ok =
                n1 == 0 && n2 == 3 && n3 == (MSG.len() - 1) as u64
                    && nbw == err(EINVAL) && nbf == err(EBADF);

            // Clean any leftovers so the suite replays on a reused image.
            for t in [RF, RF2, RF3] {
                let _ = syscall3(SYS_UNLINK, t.as_ptr() as u64, t.len() as u64, 0);
            }
            // Create RF, write it, rename RF -> RF2: source must vanish and
            // the new name must appear.
            let rf = syscall3(SYS_OPEN, RF.as_ptr() as u64, RF.len() as u64, 0);
            let _ = syscall3(SYS_WRITE, rf, DEEPMSG.as_ptr() as u64, DEEPMSG.len() as u64);
            let _ = syscall3(SYS_CLOSE, rf, 0, 0);
            let rk = syscall4(
                SYS_RENAME,
                RF.as_ptr() as u64, RF.len() as u64,
                RF2.as_ptr() as u64, RF2.len() as u64,
            );
            let r2hit = syscall3(SYS_STAT, RF2.as_ptr() as u64, RF2.len() as u64, st.as_mut_ptr() as u64) == 0;
            let r1gone = syscall3(SYS_STAT, RF.as_ptr() as u64, RF.len() as u64, st.as_mut_ptr() as u64) == err(ENOENT);

            // Rename onto an existing target -> -EEXIST; missing source -> -ENOENT.
            let rf3fd = syscall3(SYS_OPEN, RF3.as_ptr() as u64, RF3.len() as u64, 0);
            let _ = syscall3(SYS_CLOSE, rf3fd, 0, 0);
            let rex = syscall4(
                SYS_RENAME,
                RF2.as_ptr() as u64, RF2.len() as u64,
                RF3.as_ptr() as u64, RF3.len() as u64,
            );
            let renf = syscall4(
                SYS_RENAME,
                MP.as_ptr() as u64, MP.len() as u64,
                RF.as_ptr() as u64, RF.len() as u64,
            );
            let ren_ok = rk == 0 && r2hit && r1gone
                && rex == err(EEXIST) && renf == err(ENOENT);

            // Unlink RF3 (exists) then confirm -ENOENT; a directory -> -EISDIR;
            // a missing path -> -ENOENT.
            let unh0 = syscall3(SYS_UNLINK, RF3.as_ptr() as u64, RF3.len() as u64, 0) == 0;
            let unh1 = syscall3(SYS_UNLINK, RF3.as_ptr() as u64, RF3.len() as u64, 0) == err(ENOENT);
            let und = syscall3(SYS_UNLINK, DIR.as_ptr() as u64, DIR.len() as u64, 0) == err(EISDIR);
            let unm = syscall3(SYS_UNLINK, MP.as_ptr() as u64, MP.len() as u64, 0) == err(ENOENT);
            let un_ok = unh0 && unh1 && und && unm;

            say(if stat_ok { b"fstest: stat OK\n" } else { b"fstest: stat BAD\n" });
            say(if ts_ok { b"fstest: timestamps OK\n" } else { b"fstest: timestamps BAD\n" });
            say(if seek_ok { b"fstest: seek OK\n" } else { b"fstest: seek BAD\n" });
            say(if ren_ok { b"fstest: rename OK\n" } else { b"fstest: rename BAD\n" });
            say(if un_ok { b"fstest: unlink OK\n" } else { b"fstest: unlink BAD\n" });
            // RF2 is left behind holding DEEPMSG after the successful rename.
            stat_ok && ts_ok && seek_ok && ren_ok && un_ok
        };

        say(if ok && ok2 && badptr_checks && ok4 && errno_checks && c3_checks && fsync_checks { PASS } else { FAIL });
        syscall3(
            SYS_EXIT,
            if ok && ok2 && badptr_checks && ok4 && errno_checks && c3_checks && fsync_checks { 0 } else { 1 },
            0,
            0,
        );
    }
    loop {
        core::hint::spin_loop();
    }
}
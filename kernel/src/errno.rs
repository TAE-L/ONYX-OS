//! C2: Linux errno values for syscall error returns.
//!
//! Convention (matches Linux x86-64): a failing syscall returns `-errno` in
//! rax (two's-complement negative — e.g. ENOENT → `0xFFFF_FFFF_FFFF_FFFE`);
//! success returns a small non-negative value. Ring 3 checks
//! `(ret as i64) < 0`. The old M4 sentinel `u64::MAX` (= -1 = -EPERM) still
//! decodes as *a* generic error under this rule, so the only pre-C2 callers
//! that break are the ones comparing against the exact sentinel — those are
//! migrated to `is_err` in the same change.

/// Operation not permitted.
pub const EPERM: u64 = 1;
/// No such file or directory.
pub const ENOENT: u64 = 2;
/// No such process.
pub const ESRCH: u64 = 3;
/// I/O error.
pub const EIO: u64 = 5;
/// Argument list too long (used for oversized spawn images).
pub const E2BIG: u64 = 7;
/// Exec format error (not a valid executable image).
pub const ENOEXEC: u64 = 8;
/// Bad file descriptor.
pub const EBADF: u64 = 9;
/// No child process.
pub const ECHILD: u64 = 10;
/// Resource temporarily unavailable (would block).
pub const EAGAIN: u64 = 11;
/// Out of memory.
pub const ENOMEM: u64 = 12;
/// Bad address (user-pointer validation failed).
pub const EFAULT: u64 = 14;
/// File exists.
pub const EEXIST: u64 = 17;
/// Is a directory (e.g. unlink/rename of a directory where a file op is needed).
pub const EISDIR: u64 = 21;
/// Invalid argument.
pub const EINVAL: u64 = 22;
/// Too many open files (per-task fd table full).
pub const EMFILE: u64 = 24;
/// No space left on device.
pub const ENOSPC: u64 = 28;
/// File name too long.
pub const ENAMETOOLONG: u64 = 36;
/// Invalid (unimplemented) syscall number.
pub const ENOSYS: u64 = 38;

/// Encode an errno for a syscall return: rax = -errno (Linux convention).
#[inline]
pub fn err(e: u64) -> u64 {
    (e as i64).wrapping_neg() as u64
}
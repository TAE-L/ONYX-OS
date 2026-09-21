//! Kernel-service lock (KSL) — M9.8-(d).
//!
//! Before M9.8-(d) the kernel kept its remaining global structures safe by
//! *construction*: `scheduler::plan_switch` only ever ran a task on the CPU
//! that spawned it (CPU affinity), so every structure that was not per-CPU —
//! the frame allocator, the input/keyboard/mouse rings, the framebuffer's
//! cursor machinery, the file-system stack — was effectively single-threaded
//! even with `-smp 4`.
//!
//! (d) removes that affinity so tasks can migrate, which turns every one of
//! those structures into a genuine shared resource. The KSL is the single,
//! explicitly-named boundary they are now serialized by. It is intentionally
//! *one* lock (a "big kernel lock", the same first step Linux took): correctness
//! first, and it keeps the lock order trivially acyclic — the KSL may be taken
//! inside a scheduler-critical section or nested under a subsystem mutex, but no
//! subsystem lock may be taken *while holding the KSL*, except the ones owned by
//! that subsystem itself (e.g. `blkcache::CACHE`, `block::DISK`,
//! `vfs::MOUNTS`), which are always acquired after it.
//!
//! Rules for holders:
//!   * acquisition disables interrupts on this CPU and spins on the lock, so
//!     the critical section must be **bounded** — no waiting for a device, for
//!     a line of input, for a child process, or for `sleep`/`yield` (those go
//!     *around* the KSL, never inside it);
//!   * the lock is **not** reentrant — `debug_assert`ed, so a nested
//!     acquisition in a debug build panics instead of deadlocking;
//!   * the guard re-enables interrupts *after* releasing the lock (same
//!     discipline as `scheduler::SchedGuard`) so no IRQ can observe a
//!     half-released section.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The lock itself. Held for bounded, non-blocking sections only.
static KSL: spin::Mutex<()> = spin::Mutex::new(());

/// CPU index currently holding the KSL (`usize::MAX` = free). Only used for the
/// reentrancy assertion and for error messages.
static HOLDER: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Total acquisitions (test/observability counter, also asserted > 0).
static ACQUISITIONS: AtomicU64 = AtomicU64::new(0);

/// Kernel-service critical section guard.
pub struct KslGuard {
    if_was_on: bool,
    guard: Option<spin::MutexGuard<'static, ()>>,
}

impl Drop for KslGuard {
    fn drop(&mut self) {
        drop(self.guard.take()); // release the lock FIRST
        HOLDER.store(usize::MAX, Ordering::Release);
        if self.if_was_on {
            x86_64::instructions::interrupts::enable();
        }
    }
}

/// Enter a kernel-service critical section (IF=0 + the KSL).
#[inline]
pub fn lock() -> KslGuard {
    let me = crate::smp::cpu_index();
    debug_assert_ne!(
        HOLDER.load(Ordering::Acquire),
        me,
        "KSL re-entered on cpu {me} - the lock is not reentrant"
    );
    let if_was_on = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let guard = KSL.lock();
    HOLDER.store(me, Ordering::Release);
    ACQUISITIONS.fetch_add(1, Ordering::Relaxed);
    KslGuard {
        if_was_on,
        guard: Some(guard),
    }
}

/// Is this CPU inside a KSL section? (debug assertions in callers)
pub fn held_by_this_cpu() -> bool {
    HOLDER.load(Ordering::Acquire) == crate::smp::cpu_index()
}

/// Total KSL acquisitions since boot (observability for the test suites).
pub fn acquisitions() -> u64 {
    ACQUISITIONS.load(Ordering::Relaxed)
}

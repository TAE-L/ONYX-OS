//! P16-0.1: SMEP + SMAP - supervisor-mode execution/access prevention.
//!
//! # What these actually stop
//!
//! Both close a hole created by *sharing one address space*. While ring-3 and
//! the kernel are mapped into the same page tables (which OnyxOS is today - see
//! `userspace.rs:26`, per-process CR3 is Phase 3.1) - a ring-3 program can point
//! a data pointer at a kernel address and the kernel will happily read or write
//! it. That turns any kernel bug handling user-controlled pointers into an
//! arbitrary kernel read/write primitive.
//!
//!   * **SMEP** (CR4 bit 20) - a `#PF` on any *instruction fetch* from a page
//!     that is also user-accessible. Ring-3 can no longer *execute* kernel code,
//!     removing classic ret2usr / ROP-into-kernel payloads.
//!   * **SMAP** (CR4 bit 21) - a `#PF` on any supervisor-mode *data* access to a
//!     page that is user-accessible. This is the one that matters most here,
//!     because the shared address space means user pages and kernel mappings
//!     overlap constantly.
//!
//! Both are **hardware-enforced** with near-zero cost: no TLB penalty, no
//! software check, and they cannot be bypassed by a kernel bug.
//!
//! # SMAP found real bugs on day one
//!
//! Enabling it immediately faulted the boot in `userspace::init` with
//! `PROTECTION_VIOLATION | CAUSED_BY_WRITE`. The ELF segment copy, the exit
//! trampoline and the initial user stack are all deliberate supervisor writes to
//! USER_ACCESSIBLE pages that had never been announced. Those now run inside an
//! explicit [`with_stac`] window. That is the feature doing its job: it turned
//! "these writes are unprotected" from an assumption into a `#PF`.
//!
//! # The `AC` bypass, and why this file is not a one-liner
//!
//! SMEP/SMAP are **not enforced** for a supervisor access when RFLAGS.AC is set.
//! That is how the architectural kernel-mode accesses (`MOV` from CR, `LGDT`,
//! interrupt state saves) keep working - and it means a kernel that leaves AC
//! set would silently lose the protection. [`with_stac`] uses `STAC`/`CLAC`
//! rather than `pushfq`/`popfq` precisely so it toggles **only** AC, and the
//! syscall entry stub has an SMAP variant that brackets `dispatch` in one such
//! window (`syscall::SYSCALL_STAC_FRAMES` reports which stub is installed).
//!
//! # Detection
//!
//! Feature bits come from `CPUID.(EAX=07H,ECX=0):EBX` bit 7 (SMEP) and bit 20
//! (SMAP). Both are reported by every CPU QEMU emulates with `-cpu max`, but the
//! default `qemu64` model does NOT advertise them - so detection is
//! unconditional rather than assumed: writing a reserved CR4 bit raises `#GP`,
//! and a hardcoded write is not safe on an older CPU.

use core::sync::atomic::{AtomicBool, Ordering};
use x86_64::registers::control::{Cr4, Cr4Flags};

/// The CPU advertises SMEP (`CPUID.07H:EBX[7]`).
static SMEP_OK: AtomicBool = AtomicBool::new(false);
/// The CPU advertises SMAP (`CPUID.07H:EBX[20]`).
static SMAP_OK: AtomicBool = AtomicBool::new(false);
/// CR4.SMEP is actually set now (verified by read-back).
static SMEP_ON: AtomicBool = AtomicBool::new(false);
/// CR4.SMAP is actually set now (verified by read-back).
static SMAP_ON: AtomicBool = AtomicBool::new(false);

/// `CPUID.(EAX=07H,ECX=0):EBX` bit 7 - supervisor-mode execution prevention.
const CPUID7_EBX_SMEP: u32 = 1 << 7;
/// `CPUID.(EAX=07H,ECX=0):EBX` bit 20 - supervisor-mode access prevention.
const CPUID7_EBX_SMAP: u32 = 1 << 20;

/// Raw `CPUID`, saving EBX around it because LLVM reserves that register.
///
/// Same trick as `fpu::cpuid` and `time::cpuid`: the compiler may hold a value
/// in RBX across the instruction, and `cpuid` clobbers it.
unsafe fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let (eax, ebx, ecx, edx);
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov r10d, ebx",
            "pop rbx",
            inlateout("eax") leaf => eax,
            inlateout("ecx") sub => ecx,
            lateout("r10d") ebx,
            lateout("edx") edx,
            options(nostack, preserves_flags)
        );
    }
    (eax, ebx, ecx, edx)
}

/// Is SMAP actually on? Read by `syscall::program_msrs` to choose the entry
/// stub (`stac` would `#UD` on a CPU without it).
pub fn smap_enabled() -> bool {
    SMAP_ON.load(Ordering::Acquire)
}

/// What SMEP/SMAP support this CPU has, and what we ended up turning on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The CPU reports SMEP support.
    pub smep_supported: bool,
    /// The CPU reports SMAP support.
    pub smap_supported: bool,
    /// SMEP is set in CR4 (implies `smep_supported`).
    pub smep_enabled: bool,
    /// SMAP is set in CR4 (implies `smap_supported`).
    pub smap_enabled: bool,
    /// The installed syscall stub brackets `dispatch` in an SMAP window.
    pub syscall_stac_safe: bool,
}

pub fn status() -> Status {
    Status {
        smep_supported: SMEP_OK.load(Ordering::Acquire),
        smap_supported: SMAP_OK.load(Ordering::Acquire),
        smep_enabled: SMEP_ON.load(Ordering::Acquire),
        smap_enabled: SMAP_ON.load(Ordering::Acquire),
        syscall_stac_safe: crate::syscall::SYSCALL_STAC_FRAMES.load(Ordering::Acquire),
    }
}

/// Run `f` with an explicit SMAP window open (RFLAGS.AC set), then closed.
///
/// This is the sanctioned way for supervisor code to touch a user page on
/// purpose. SMAP blocks *accidental* supervisor access to user-accessible pages;
/// the deliberate cases (copying a syscall result out, loading an ELF segment)
/// are what this is for.
///
/// Implemented with `STAC`/`CLAC` rather than `pushfq`/`and`/`popfq`, because
/// STAC/CLAC set and clear **exactly** RFLAGS.AC - a pushfq/popfq pair would
/// round-trip every other flag through the stack and could resurrect bits the
/// rest of the kernel deliberately cleared.
///
/// On a CPU without SMAP it is a plain call: nothing is enforced, so the window
/// is unnecessary, and `stac` would `#UD`.
///
/// # Caveat
///
/// The window covers the WHOLE closure, so keep it as small as possible: it
/// disables SMAP for everything it does, not just the intended access. Narrowing
/// the syscall path from "the entire `dispatch`" (what `syscall_entry_smap` does
/// today) down to per-`copy_to_user` windows is planned work, not an oversight.
#[inline]
pub fn with_stac<R>(f: impl FnOnce() -> R) -> R {
    if smap_enabled() {
        // SAFETY: `stac`/`clac` are defined in supervisor mode on a CPU that
        // reports SMAP (guarded above). They touch no memory and preserve every
        // flag except AC.
        unsafe {
            core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        }
        let out = f();
        unsafe {
            core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
        }
        out
    } else {
        f()
    }
}

/// Detect support and enable what the CPU allows, then report the outcome.
///
/// Runs in the boot path with interrupts off, before any ring-3 task exists, so
/// nothing can fault while the bits are being set. Call once, after `fpu::init`
/// and before `syscall::init` (which consults [`smap_enabled`] to pick its stub).
///
/// Cannot fail fatally: an unsupported bit is *skipped*, never written.
pub fn init() -> Status {
    // `CPUID.07H` is only meaningful if the max basic leaf is >= 7. Querying an
    // unimplemented leaf returns garbage on some parts, so the maximum is
    // checked first (EAX of `CPUID.0`).
    let (max_leaf, _, _, _) = unsafe { cpuid(0, 0) };
    if max_leaf >= 7 {
        let (_, ebx, _, _) = unsafe { cpuid(7, 0) };
        SMEP_OK.store(ebx & CPUID7_EBX_SMEP != 0, Ordering::Release);
        SMAP_OK.store(ebx & CPUID7_EBX_SMAP != 0, Ordering::Release);
    }

    // `Cr4::read()` + `Cr4::write()` is a read-modify-write that PRESERVES the
    // reserved fields (and therefore the bootloader's PE/PAE/MCE), unlike a raw
    // write. There is no `update_flags` in this crate version.
    let mut flags = Cr4::read();
    if SMEP_OK.load(Ordering::Acquire) {
        flags.insert(Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION);
    }
    if SMAP_OK.load(Ordering::Acquire) {
        flags.insert(Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION);
    }
    // SAFETY: only bits already set (plus the two CPUID-confirmed feature bits)
    // are written; `Cr4::write` masks to `Cr4Flags::all()` and re-ORs the
    // reserved ones, so this cannot clear PAE/PE out from under paging.
    unsafe { Cr4::write(flags) }

    // Read CR4 back rather than assuming the write landed. `Cr4::write` cannot
    // report failure, so this is the only way to tell "enabled" from "believed
    // enabled", and the boot log is the only place that differs.
    let cr4 = Cr4::read_raw();
    SMEP_ON.store(cr4 & (1 << 20) != 0, Ordering::Release);
    SMAP_ON.store(cr4 & (1 << 21) != 0, Ordering::Release);

    status()
}

/// Log what was detected and enabled, one `[sec]` line each so a `test-*.ps1`
/// can assert on it.
pub fn report() {
    let s = status();
    crate::serial_writeln!(
        "[sec] SMEP: cpu={} cr4={}",
        if s.smep_supported { "yes" } else { "no" },
        if s.smep_enabled { "on" } else { "off" }
    );
    crate::serial_writeln!(
        "[sec] SMAP: cpu={} cr4={}",
        if s.smap_supported { "yes" } else { "no" },
        if s.smap_enabled { "on" } else { "off" }
    );
    // Reported again from `syscall::init` once the stub is actually installed;
    // this early read is necessarily `false` on the boot CPU because
    // `syscall::init` has not run yet.
    crate::serial_writeln!(
        "[sec] syscall STAC safe: {} (before syscall::init)",
        s.syscall_stac_safe
    );
}

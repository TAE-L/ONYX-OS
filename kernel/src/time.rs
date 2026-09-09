//! M9.6-A1: high-resolution timekeeping.
//!
//! A monotonic nanosecond clock built on the x86 Time Stamp Counter,
//! calibrated at boot against the 100 Hz PIT tick counter (no new hardware:
//! the PIT is already running for preemption, so 10 ticks give the TSC
//! frequency to ~1%).
//!
//! The TSC is read with `rdtsc` (via the `x86_64` crate). CPUID leaf
//! 0x8000_0007 EDX bit 8 ("Invariant TSC") tells us the rate is constant
//! and independent of core clock changes — on QEMU's default models this is
//! reported and the frequency is stable, so a one-shot calibration at boot
//! is sufficient. Without invariance the clock still works, just with the
//! usual TSC caveat (documented, and irrelevant inside QEMU).
//!
//! Wall time is anchored once at boot from the RTC (seconds-of-day, as the
//! RTC driver exposes); `realtime_ns` advances that anchor with the
//! monotonic clock (it wraps at midnight — fine for a benchmarking clock).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static CALIBRATED: AtomicBool = AtomicBool::new(false);
/// CPUID-reported Invariant TSC support.
static INVARIANT: AtomicBool = AtomicBool::new(false);
/// Measured TSC frequency in Hz (cycles per second).
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// TSC value at calibration time; `now_ns` measures deltas from here.
static BASE_TSC: AtomicU64 = AtomicU64::new(0);
/// RTC seconds-of-day captured at boot (wall-clock anchor).
static BOOT_SOD: AtomicU64 = AtomicU64::new(0);

/// Raw `rdtsc` (the `x86_64` crate has no tsc module on this version).
#[inline]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u64) << 32) | lo as u64
}

/// Raw CPUID (leaf in EAX). EBX is reserved by LLVM, so it is saved/restored
/// around the instruction and captured via a scratch register.
unsafe fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov r10d, ebx",
            "pop rbx",
            inlateout("eax") leaf => eax,
            out("r10d") ebx,
            lateout("ecx") ecx,
            lateout("edx") edx,
            options(nomem)
        );
    }
    (eax, ebx, ecx, edx)
}

/// Detect + calibrate. Call once at boot AFTER interrupts are enabled (the
/// calibration counts PIT ticks, which requires the timer IRQ to run).
pub fn init() {
    BOOT_SOD.store(crate::rtc::now_secs_of_day() as u64, Ordering::Relaxed);

    // Invariant TSC: CPUID 0x8000_0007 EDX bit 8 (guard the extended-leaf max).
    let max_ext = unsafe { cpuid(0x8000_0000).0 };
    let invariant =
        max_ext >= 0x8000_0007 && (unsafe { cpuid(0x8000_0007).3 } & (1 << 8)) != 0;
    INVARIANT.store(invariant, Ordering::Relaxed);

    // Calibrate over 10 PIT ticks (~100 ms). Bounded spins so a dead timer
    // can never hang the boot: on timeout the clock stays uncalibrated and
    // SYS_GETTIME reports 0 (callers treat that as "no clock yet").
    let t0 = crate::pit::ticks();
    let mut spin = 0u64;
    while crate::pit::ticks() == t0 {
        spin += 1;
        if spin > 500_000_000 {
            return;
        }
        core::hint::spin_loop();
    }
    let t1 = crate::pit::ticks();
    let c1 = rdtsc();
    spin = 0;
    while crate::pit::ticks() - t1 < 10 {
        spin += 1;
        if spin > 2_000_000_000 {
            return;
        }
        core::hint::spin_loop();
    }
    let t2 = crate::pit::ticks();
    let c2 = rdtsc();

    let dt_ticks = t2 - t1; // ~10 ticks = ~100 ms
    let dcycles = c2 - c1;
    if dt_ticks == 0 || dcycles == 0 {
        return;
    }
    let hz = dcycles * u64::from(crate::pit::TICK_HZ) / dt_ticks;
    TSC_HZ.store(hz, Ordering::Relaxed);
    BASE_TSC.store(c1, Ordering::Relaxed);
    CALIBRATED.store(true, Ordering::Relaxed);
    crate::serial_writeln!(
        "time: TSC calibrated: {} Hz over {} PIT ticks (invariant: {})",
        hz,
        dt_ticks,
        invariant
    );
}

/// Monotonic nanoseconds since calibration (0 = not calibrated yet).
pub fn now_ns() -> u64 {
    if !CALIBRATED.load(Ordering::Relaxed) {
        return 0;
    }
    let cycles = rdtsc().wrapping_sub(BASE_TSC.load(Ordering::Relaxed));
    let hz = TSC_HZ.load(Ordering::Relaxed) as u128;
    // u128 intermediate: cycles * 1e9 would overflow u64 within ~6 s @ 3 GHz.
    ((cycles as u128 * 1_000_000_000) / hz) as u64
}

/// Wall-clock nanoseconds: RTC anchor (seconds-of-day at boot) advanced by
/// the monotonic clock. Wraps at midnight (documented limitation).
pub fn realtime_ns() -> u64 {
    BOOT_SOD.load(Ordering::Relaxed) * 1_000_000_000 + now_ns()
}

/// Measured TSC frequency (0 = uncalibrated).
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

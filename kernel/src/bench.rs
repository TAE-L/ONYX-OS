//! M9.8 parallelism benchmark: does the SMP scheduler actually shorten work?
//!
//! The task spawns `WORKERS` CPU-bound workers and measures the wall-clock
//! time until all of them finish a fixed number of integer iterations. Under
//! task migration + work stealing (M9.8-d/e) the workers should distribute
//! across the cores and the wall clock should drop roughly with the CPU
//! count; without it every worker runs serially on the boot CPU. The
//! `test-para.ps1` harness boots the same image at `-smp 1` and `-smp 4`,
//! parses the `[para]` markers and asserts a real speedup.
//!
//! The work is deliberately *plain integer arithmetic* through
//! `core::hint::black_box`: no FPU (so the run measures pure scheduling
//! parallelism, not the XSAVE path — that is `test-avx.ps1`'s job), no memory
//! allocation, no locks (a lock-heavy bench would measure lock overhead, not
//! parallelism).

use core::hint::black_box;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Workers to run. Equal to the `-smp 4` CPU count so a perfect run uses
/// every core; on `-smp 1` the same four workers simply serialize.
const WORKERS: usize = 4;
/// Task-table ids of the spawned workers (registered by `task` after spawn).
/// Used by the scheduler's switch tracer (`[psw]` lines) while debugging
/// steal behavior; flip `TRACE` to `false` for quiet boots.
static W_IDS: [AtomicUsize; 8] = [
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
];
static W_SLOT: AtomicUsize = AtomicUsize::new(0);
/// When 1, the scheduler logs every switch involving a bench worker
/// (`[psw]` lines) — steal-behavior debugging only.
pub static TRACE: AtomicUsize = AtomicUsize::new(0);

/// True when `idx` is one of this run's bench workers (scheduler tracer).
pub fn is_bench_worker(idx: usize) -> bool {
    for w in W_IDS.iter() {
        if w.load(Ordering::Relaxed) == idx {
            return true;
        }
    }
    false
}
/// Iterations per worker. Tuned for TCG (~1-2M plain adds per ms with the
/// `black_box` store): ~40 ms of work per worker (~160 ms serial total) — long
/// enough to be pre-empted and migrated many times over, short enough that the
/// whole bench finishes well inside the boot window.
const UNITS: u64 = 20_000_000;

/// Completion slot handout: each worker claims 0..WORKERS on finish.
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
/// Per-worker checksums (`seed + UNITS * STEP` wrapping), for validity.
static RES: [AtomicU64; 8] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// Per-worker first-seen CPU index — the distribution evidence.
static CPUS: [AtomicUsize; 8] = [
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(usize::MAX),
];

/// Worker body: `UNITS` dependent adds so the loop cannot be unrolled or
/// skipped; the checksum proves every iteration ran.
fn worker() {
    let cpu = crate::smp::cpu_index();
    let id = crate::scheduler::current_index();
    let wslot = W_SLOT.fetch_add(1, Ordering::Relaxed);
    if wslot < W_IDS.len() {
        W_IDS[wslot].store(id, Ordering::Relaxed);
    }
    crate::serial_writeln!("[para] w{} start cpu={}", id, cpu);
    let mut acc: u64 = 12345;
    for _ in 0..UNITS {
        acc = acc.wrapping_add(7);
        black_box(acc);
    }
    let slot = NEXT_SLOT.fetch_add(1, Ordering::Release);
    if slot < CPUS.len() {
        // Record the CPU at COMPLETION, not at start: the claim-then-migrate
        // pattern means all workers may *start* on the same CPU, but with real
        // parallel execution they finish on different cores simultaneously.
        CPUS[slot].store(crate::smp::cpu_index(), Ordering::Relaxed);
        RES[slot].store(acc, Ordering::Release);
    }
    crate::serial_writeln!(
        "[para] w{} done cpu={} slot={} t={} ms",
        id,
        crate::smp::cpu_index(),
        slot,
        crate::apic::ms_since_boot()
    );
}

/// Bench driver: spawn the workers, wait for the completion handouts, report.
pub fn task() {
    NEXT_SLOT.store(0, Ordering::Relaxed);
    for i in 0..WORKERS {
        RES[i].store(0, Ordering::Relaxed);
        CPUS[i].store(usize::MAX, Ordering::Relaxed);
    }
    // Wait for the AP bring-up to finish so the measurement always covers the
    // full CPU count (on `-smp 4` the bring-up task runs alongside the early
    // boot tasks). Bounded: a bring-up failure must not hang the bench — the
    // run then just measures whatever cores are online.
    let mut waited = 0;
    while !crate::smp::BRINGUP_DONE.load(Ordering::Acquire) && waited < 8_000 {
        crate::scheduler::sleep_kernel(50);
        waited += 50;
    }
    crate::serial_writeln!(
        "[para] bench start workers={} units={} (smp cpus online: {}, task {})",
        WORKERS,
        UNITS,
        crate::smp::online_cpus(),
        crate::scheduler::current_index()
    );
    for _ in 0..WORKERS {
        crate::scheduler::spawn_prio(worker, crate::scheduler::PRIO_NORMAL);
    }
    let t0 = crate::apic::ms_since_boot();
    loop {
        crate::scheduler::sleep_kernel(10);
        if NEXT_SLOT.load(Ordering::Acquire) >= WORKERS {
            break;
        }
    }
    let elapsed = crate::apic::ms_since_boot().saturating_sub(t0);
    let mut cpus = [0usize; 8];
    let mut ok = 1u8;
    let expected = 12345u64.wrapping_add(UNITS.wrapping_mul(7));
    for i in 0..WORKERS {
        cpus[i] = CPUS[i].load(Ordering::Relaxed);
        if RES[i].load(Ordering::Relaxed) != expected {
            ok = 0;
        }
    }
    // One compact line the harness parses, plus the distribution as evidence.
    crate::serial_writeln!(
        "[para] bench done elapsed={} ms ok={} cpus=[{},{},{},{}]",
        elapsed,
        ok,
        cpus[0],
        cpus[1],
        cpus[2],
        cpus[3]
    );
    crate::scheduler::exit_current();
}

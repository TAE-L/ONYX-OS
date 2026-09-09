//! M9.6-B5: latency + throughput instrumentation — the "ultrafast" feedback
//! loop.
//!
//! Lock-free counters + ns-bucket histograms touched from IRQ handlers (IF=0),
//! the scheduler, and syscall dispatch; a snapshot renderer consumed by the
//! boot-time report, the shell's `perf` command (SYS_PERF), and the test
//! scripts. Everything is approximate-by-design (rdtsc, coarse buckets) but
//! cheap: two rdtsc + one atomic add per measured event, no allocation on the
//! hot path.
//!
//! Measured:
//!   * context switches        (preempt's switch path)
//!   * syscalls                (total + per-number, dispatch duration hist)
//!   * LAPIC timer IRQs        (firing lateness vs. the 1 ms schedule +
//!                              handler service cost, excluding preempt)
//!   * PIT / keyboard / mouse  (handler service cost)
//!   * block cache             (hit rate from blkcache::stats)
//!   * kernel heap             (used/free from the linked-list allocator)
//!
//! Latency buckets are logarithmic-ish from 100 ns to 10 ms — chosen to
//! resolve sub-microsecond IRQ service cost AND multi-millisecond serial/
//! disk stalls in the same 17-slot histogram.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Raw counters
// ---------------------------------------------------------------------------

static CTX_SWITCHES: AtomicU64 = AtomicU64::new(0);
static SYSCALL_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Per-syscall-number counts (nr < 32; higher numbers just hit the total).
static SYSCALL_BY_NR: [AtomicU64; 32] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; 32]
};

/// Context switch (an actual `context_switch` call, not a keep-current tick).
pub fn ctx_switch() {
    CTX_SWITCHES.fetch_add(1, Ordering::Relaxed);
}

/// Syscall dispatch entry: count it, return the entry TSC for `syscall_exit`.
pub fn syscall_enter(nr: u64) -> u64 {
    SYSCALL_TOTAL.fetch_add(1, Ordering::Relaxed);
    if (nr as usize) < SYSCALL_BY_NR.len() {
        SYSCALL_BY_NR[nr as usize].fetch_add(1, Ordering::Relaxed);
    }
    rdtsc()
}

/// Syscall dispatch exit: record the duration.
pub fn syscall_exit(start_tsc: u64) {
    let ns = cycles_to_ns(rdtsc().wrapping_sub(start_tsc));
    if ns > 0 {
        SYSCALL_DUR.record(ns);
    }
}

// ---------------------------------------------------------------------------
// Histograms (17 logarithmic-ish ns buckets + exact count + exact ns sum)
// ---------------------------------------------------------------------------

pub const HIST_BUCKETS: usize = 17;
/// Lower edge (ns) of each bucket. Bucket i covers [LO[i], LO[i+1]); the last
/// bucket covers [LO[16], inf).
const BUCKET_LO: [u64; HIST_BUCKETS] = [
    0,
    100,
    250,
    500,
    1_000,
    2_000,
    5_000,
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
];

pub struct Hist {
    buckets: [AtomicU64; HIST_BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicU64,
}

impl Hist {
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);

    pub const fn new() -> Self {
        Self {
            buckets: [Self::Z; HIST_BUCKETS],
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
        }
    }

    /// Record one duration (ns). IF-safe: atomics only.
    pub fn record(&self, ns: u64) {
        let mut i = 0;
        while i + 1 < HIST_BUCKETS && ns >= BUCKET_LO[i + 1] {
            i += 1;
        }
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ([u64; HIST_BUCKETS], u64, u64) {
        let mut b = [0u64; HIST_BUCKETS];
        for (i, slot) in self.buckets.iter().enumerate() {
            b[i] = slot.load(Ordering::Relaxed);
        }
        (
            b,
            self.count.load(Ordering::Relaxed),
            self.sum_ns.load(Ordering::Relaxed),
        )
    }

    /// (count, min_ns, avg_ns, p99_ns, max_ns) — min/max/p99 derived from the
    /// bucket edges (approximate), avg exact from the ns sum.
    fn summary(&self) -> (u64, u64, u64, u64, u64) {
        let (b, count, sum) = self.snapshot();
        if count == 0 {
            return (0, 0, 0, 0, 0);
        }
        // Upper edge of bucket i (the last bucket has no finite upper edge —
        // indexing BUCKET_LO[i + 1] there panicked: "len is 17, index 17").
        let hi = |i: usize| -> u64 {
            if i + 1 < HIST_BUCKETS {
                BUCKET_LO[i + 1].saturating_sub(1)
            } else {
                u64::MAX
            }
        };
        let mut min = BUCKET_LO[HIST_BUCKETS - 1];
        for (i, &c) in b.iter().enumerate() {
            if c > 0 {
                min = BUCKET_LO[i];
                break;
            }
        }
        let mut max = 0;
        for (i, &c) in b.iter().enumerate().rev() {
            if c > 0 {
                max = hi(i);
                break;
            }
        }
        let avg = sum / count;
        let target = count.saturating_sub(count / 100).max(1); // 99th percentile
        let mut cum = 0u64;
        let mut p99 = max;
        for (i, &c) in b.iter().enumerate() {
            cum += c;
            if cum >= target {
                p99 = hi(i);
                break;
            }
        }
        (count, min, avg, p99, max)
    }
}

static TIMER_LATE: Hist = Hist::new();
static TIMER_DUR: Hist = Hist::new();
static KBD_DUR: Hist = Hist::new();
static MOUSE_DUR: Hist = Hist::new();
static SYSCALL_DUR: Hist = Hist::new();

/// Raw rdtsc (same shape as time::rdtsc, which is private there).
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

/// TSC cycles -> ns (0 when the clock is not calibrated yet).
fn cycles_to_ns(cycles: u64) -> u64 {
    let hz = crate::time::tsc_hz() as u128;
    if hz == 0 {
        return 0;
    }
    ((cycles as u128 * 1_000_000_000) / hz) as u64
}

// ---------------------------------------------------------------------------
// IRQ hooks (called from `extern "x86-interrupt"` handlers — IF=0, hot path)
// ---------------------------------------------------------------------------

/// TSC timestamp of the previous LAPIC timer tick (0 = no tick seen yet).
static LAST_TICK_TSC: AtomicU64 = AtomicU64::new(0);

/// LAPIC timer entry: measures how LATE this tick fired vs. the 1 ms schedule
/// (the visible jitter a game feels as input/frame pacing noise) and returns
/// the entry TSC for `irq_timer_exit`.
pub fn irq_timer_enter() -> u64 {
    let now = rdtsc();
    let last = LAST_TICK_TSC.swap(now, Ordering::Relaxed);
    if last != 0 {
        let expected = last + crate::time::tsc_hz() / 1000; // 1 ms in cycles
        let late = now.saturating_sub(expected);
        let ns = cycles_to_ns(late);
        if ns > 0 {
            TIMER_LATE.record(ns);
        }
    }
    now
}

/// LAPIC timer exit: records the handler SERVICE cost (enter -> eoi). Called
/// BEFORE `preempt` — the switch-away time belongs to the descheduled task,
/// not to the interrupt.
pub fn irq_timer_exit(start_tsc: u64) {
    let ns = cycles_to_ns(rdtsc().wrapping_sub(start_tsc));
    if ns > 0 {
        TIMER_DUR.record(ns);
    }
}

/// Keyboard IRQ entry (returns the entry TSC).
pub fn irq_kbd_enter() -> u64 {
    rdtsc()
}

/// Keyboard IRQ exit (full handler cost — no preemption inside).
pub fn irq_kbd_exit(start_tsc: u64) {
    let ns = cycles_to_ns(rdtsc().wrapping_sub(start_tsc));
    if ns > 0 {
        KBD_DUR.record(ns);
    }
}

/// Mouse IRQ entry (returns the entry TSC).
pub fn irq_mouse_enter() -> u64 {
    rdtsc()
}

/// Mouse IRQ exit (full handler cost — no preemption inside).
pub fn irq_mouse_exit(start_tsc: u64) {
    let ns = cycles_to_ns(rdtsc().wrapping_sub(start_tsc));
    if ns > 0 {
        MOUSE_DUR.record(ns);
    }
}

// ---------------------------------------------------------------------------
// Snapshot rendering (task/syscall context only — allocates for the strings)
// ---------------------------------------------------------------------------

/// Format ns as a compact human unit ("850ns", "12.4us", "3.2ms").
fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{}.{:02}ms", ns / 1_000_000, (ns % 1_000_000) / 10_000)
    } else if ns >= 1_000 {
        format!("{}.{:02}us", ns / 1_000, (ns % 1_000) / 10)
    } else {
        format!("{}ns", ns)
    }
}

/// One-line hist summary: "n=1234 min=850ns avg=1.2us p99=9.8us max=42us".
fn fmt_hist(name: &str, h: &Hist) -> String {
    let (count, min, avg, p99, max) = h.summary();
    if count == 0 {
        return format!("[perf] {name}: n=0");
    }
    // The last bucket is open-ended: print ">10ms" instead of u64::MAX.
    let max_s = if max == u64::MAX {
        String::from(">10ms")
    } else {
        fmt_ns(max)
    };
    let p99_s = if p99 == u64::MAX {
        String::from(">10ms")
    } else {
        fmt_ns(p99)
    };
    format!(
        "[perf] {name}: n={} min={} avg={} p99={} max={}",
        count,
        fmt_ns(min),
        fmt_ns(avg),
        p99_s,
        max_s
    )
}

/// Render the full performance snapshot. Called from task/syscall context
/// (allocates); NOT from IRQ handlers.
pub fn render_lines() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let up_ms = crate::apic::ms_since_boot().max(1);
    let switches = CTX_SWITCHES.load(Ordering::Relaxed);
    v.push(format!(
        "[perf] uptime {} ms | tsc {} Hz | ctx switches {} ({} /s)",
        up_ms,
        crate::time::tsc_hz(),
        switches,
        switches * 1000 / up_ms
    ));

    // Syscalls: total + the three busiest numbers.
    let total = SYSCALL_TOTAL.load(Ordering::Relaxed);
    let mut tops: [(u64, u64); 3] = [(u64::MAX, 0); 3]; // (nr, count)
    for (nr, c) in SYSCALL_BY_NR.iter().enumerate() {
        let cnt = c.load(Ordering::Relaxed);
        if cnt == 0 {
            continue;
        }
        // Insert into the top-3 (descending by count).
        for slot in tops.iter_mut() {
            if cnt > slot.1 {
                let tmp = *slot;
                *slot = (nr as u64, cnt);
                let mut rest = tmp;
                for slot2 in tops.iter_mut().skip(1) {
                    if rest.1 > slot2.1 {
                        let t2 = *slot2;
                        *slot2 = rest;
                        rest = t2;
                        break;
                    }
                }
                break;
            }
        }
    }
    let names = [
        "write", "exit", "open", "read", "close", "ls", "mkdir", "spawn", "_gettime",
        "getpid", "sleep", "yield", "lspci", "flush", "waitpid", "kill", "input",
        "perf",
    ];
    let mut line = format!("[perf] syscalls: {} total | top:", total);
    for (nr, cnt) in tops.iter() {
        if *cnt == 0 {
            break;
        }
        let name = names
            .get(*nr as usize)
            .copied()
            .unwrap_or("?"); 
        line.push_str(&format!(" {}={}", name, cnt));
    }
    v.push(line);
    v.push(fmt_hist("syscall dur", &SYSCALL_DUR));

    v.push(fmt_hist("timer late (1ms sched)", &TIMER_LATE));
    v.push(fmt_hist("timer irq cost", &TIMER_DUR));
    v.push(fmt_hist("kbd irq cost", &KBD_DUR));
    v.push(fmt_hist("mouse irq cost", &MOUSE_DUR));

    let (hits, misses) = crate::blkcache::stats();
    let lookups = hits + misses;
    let pct = if lookups > 0 { hits * 100 / lookups } else { 0 };
    v.push(format!(
        "[perf] blk cache: hits={} misses={} ({}% hit)",
        hits, misses, pct
    ));

    let (used, free) = crate::allocator::heap_stats();
    v.push(format!(
        "[perf] heap: used {} KiB / free {} KiB / total {} KiB",
        used / 1024,
        free / 1024,
        (used + free) / 1024
    ));

    // M9.6-A6: physical frame allocator stats (runtime snapshot; this runs
    // long after `init_global_frames`, so the snapshot always exists here).
    if let Some((frames_used, frames_total, frames_freed, frames_oom)) =
        crate::memory::frame_stats()
    {
        v.push(format!(
            "[perf] frames: used {} / free {} / total {} (4 KiB) | freed {} oom {}",
            frames_used,
            frames_total - frames_used,
            frames_total,
            frames_freed,
            frames_oom
        ));
    }
    v
}

/// Print the snapshot (serial + framebuffer console). Task/syscall context.
pub fn print_snapshot() {
    for line in render_lines() {
        crate::serial_writeln!("{}", line);
        crate::framebuffer::console_bytes(line.as_bytes());
        crate::framebuffer::console_bytes(b"\n");
    }
}
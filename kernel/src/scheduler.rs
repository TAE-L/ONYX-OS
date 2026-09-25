//! M9.6-A3: preemptive priority scheduler for kernel + ring-3 tasks.
//!
//! Three priority classes (RT > Normal > Idle), explicit time slices, and
//! full task states: Running/Ready/Sleeping/Blocked/Dead. The LAPIC timer
//! (1 ms) drives `preempt()`: it wakes due sleepers + input-blocked tasks,
//! then runs the highest-priority Ready task â€” keeping the current task
//! when nothing more urgent is ready (fewer needless context switches).
//!
//! # M9.8: multiple CPUs
//!
//! The task table is *global* (one array, one id space), so it is now guarded
//! by [`SCHED_LOCK`]. Everything that reads or writes the table takes it (with
//! interrupts off, so a holder can never be preempted while holding it â€” a
//! peer spins for microseconds, never for a full time slice).
//!
//! The context switch deliberately happens *outside* the lock: the switch
//! saves one task's stack and resumes another's, and holding a spin lock
//! across it would let a peer spin while the incoming task runs user code.
//! Two consequences shaped the design:
//!
//!   * **RSP slots are stable addresses.** The switch writes the outgoing
//!     task's RSP through a pointer held in the `Task` (a heap-allocated
//!     slot), and each CPU's idle context has its own slot in its `PerCpu`
//!     block. Nothing points into the `TASKS` vector, which may reallocate.
//!   * **A task pending a switch stays owned.** The outgoing task keeps
//!     `Running` (and its owner CPU) until the *next* `preempt` on that CPU
//!     releases it to `Ready`; otherwise another CPU could steal a task whose
//!     stack had not been saved yet.
//!
//! Scheduling itself stays deferred: any CPU that makes a task Ready (spawn,
//! wake, exit, kill, priority change) sends a reschedule IPI to the other
//! online CPUs, so an idle core never sleeps through new work.

use crate::fpu;
use crate::smp;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

const STACK_SIZE: usize = 64 * 1024;

/// Sentinel for "this CPU's idle context" (the boot/idle thread of each core).
const MAIN_INDEX: usize = smp::IDLE_INDEX;

/// Task priority classes (lower number = higher priority / more urgent).
pub const PRIO_RT: u8 = 0;
pub const PRIO_NORMAL: u8 = 1;
pub const PRIO_IDLE: u8 = 2;

/// Time slice (in 1 ms LAPIC ticks) granted each time a task is selected.
/// RT gets the minimal slice: it keeps the CPU until it blocks or yields
/// anyway (see `preempt`'s keep logic), so 1 just enables RT round-robin.
fn fresh_slice(prio: u8) -> u32 {
    match prio {
        PRIO_RT => 1,     // rotates every tick only among RT tasks
        PRIO_NORMAL => 8, // ~8 ms of work before a same-priority peer
        _ => 1,           // idle
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Ready,
    Sleeping,
    Blocked,
    Dead,
}

struct Task {
    id: u64,
    entry: fn(),
    /// Heap-allocated RSP slot (never moves): `context_switch` writes the
    /// outgoing task's stack pointer through it and reads it again on resume.
    /// Not a field of `TASKS[i]` because the vector may reallocate while the
    /// switch runs outside the scheduler lock (M9.8).
    sp_slot: *mut u64,
    stack: &'static mut [u8],
    /// Top of this task's kernel stack (16-aligned). Ring-3 interrupt/syscall
    /// entry lands here (TSS.RSP0 + syscall entry KSTACK_TOP).
    kstack_top: u64,
    state: State,
    /// CPU currently executing this task while `state == Running` (M9.8); any
    /// other state leaves the value stale and meaningless.
    owner_cpu: u8,
    priority: u8,
    /// Ticks of 1 ms remaining before a same-priority peer gets the CPU.
    slice_left: u32,
    /// Deadline (LAPIC ms) when Sleeping; woken when `now >= sleep_until_ms`.
    sleep_until_ms: u64,
    /// When Blocked, whether this task waits for a keyboard line.
    blocked_on_input: bool,
    /// M9.6-B4: when Blocked, whether this task waits for a raw input event.
    blocked_on_raw: bool,
    /// Bring-up diagnostic: sleeper already reported as >3 s overdue.
    wake_diag: bool,
    /// M9.6-B3: parent task id (0 = kernel/main â€” never a child of it).
    parent_id: u64,
    /// M9.6-B3: exit status recorded when the task becomes Dead (SYS_EXIT code,
    /// 137 for SYS_KILL).
    exit_code: u32,
    /// M9.6-B3: zombie's exit status already consumed by a parent (reaped).
    reaped: bool,
    /// M9.6-B3: this task is Blocked waiting for a child to die (waitpid).
    blocked_on_child: bool,
    /// M9.7: task runs Linux-ABI binaries â€” syscalls use Linux numbers and
    /// conventions (arg 4 in r10, `-errno` returns), served by the shim in
    /// `syscall::dispatch`.
    linux_abi: bool,
    /// 512-byte FXSAVE image swapped by `context_switch` (FPU/SSE state).
    /// Raw pointer (never null): the task list is a `static mut` Vec touched
    /// only with IF=0, so no reference may be formed through the index.
    fpu_area: *mut fpu::FpuArea,
}

static mut TASKS: Vec<Task> = Vec::new();
static mut TASK_COUNT: usize = 0;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The global scheduler lock (M9.8). Always acquired with interrupts disabled
/// (see [`sched_guard`]) so its holder cannot be preempted: a peer CPU waits
/// for the few microseconds of a pick/state change, never for a time slice.
static SCHED_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Scheduler-critical section: interrupts off + [`SCHED_LOCK`]. The lock is
/// released *before* interrupts are re-enabled, so no IRQ can observe a
/// half-released section (which would self-deadlock on this very lock).
struct SchedGuard {
    if_was_on: bool,
    guard: Option<spin::MutexGuard<'static, ()>>,
}

impl Drop for SchedGuard {
    fn drop(&mut self) {
        drop(self.guard.take()); // release the lock FIRST
        if self.if_was_on {
            x86_64::instructions::interrupts::enable();
        }
    }
}

/// Enter a scheduler-critical section (IF=0 + scheduler lock). Use in every
/// function that touches `TASKS`/`TASK_COUNT`; the guard restores the caller's
/// interrupt state on drop.
#[inline]
fn sched_guard() -> SchedGuard {
    let if_was_on = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    SchedGuard {
        if_was_on,
        guard: Some(SCHED_LOCK.lock()),
    }
}

/// Task index running on THIS CPU (MAIN_INDEX = this CPU is idle). Reads the
/// per-CPU block, so "current" is per-CPU now â€” two CPUs run two different
/// tasks at the same time.
#[inline]
fn cur_index() -> usize {
    smp::current_index()
}

/// Heap-allocated, never-moving RSP slot for a task (see `Task::sp_slot`).
fn new_sp_slot(sp: u64) -> *mut u64 {
    Box::leak(Box::new(sp))
}

/// The RSP slot of context `idx` on the CALLING CPU (MAIN_INDEX = this CPU's
/// idle context, which lives in its own block).
///
/// # Safety
/// Scheduler lock held: `TASKS` is not being mutated.
#[inline]
/// The calling task's saved-state image pointer (null for the per-CPU idle
/// context before its area exists). Diagnostics only — the scheduler lock
/// serializes TASKS access, and a diagnostic call from the running task is
/// safe because the task reads its own pointer.
pub fn current_fpu_area() -> *mut crate::fpu::FpuArea {
    unsafe { fpu_ptr(cur_index()) }
}

/// The calling task's index in the task table (`usize::MAX` in the boot
/// context before it ever became a task). Diagnostics only.
pub fn current_index() -> usize {
    cur_index()
}

unsafe fn sp_slot(idx: usize) -> *mut u64 {
    if idx == MAIN_INDEX {
        &raw mut (*smp::this_cpu()).idle_sp
    } else {
        TASKS[idx].sp_slot
    }
}

/// The FXSAVE image of context `idx` on the CALLING CPU (MAIN_INDEX = this
/// CPU's idle context).
///
/// # Safety
/// Scheduler lock held: `TASKS` is not being mutated.
#[inline]
unsafe fn fpu_ptr(idx: usize) -> *mut fpu::FpuArea {
    if idx == MAIN_INDEX {
        &raw mut (*smp::this_cpu()).idle_fpu
    } else {
        TASKS[idx].fpu_area
    }
}

/// B5 stall diagnostic: consecutive ticks the current task was KEPT although
/// it is not RT. A Normal task must lose the CPU within its 8-tick slice;
/// far beyond that means the pick found nothing Ready (state corruption /
/// mass starvation). One-shot dump of the task table to serial.
static mut STALL_TICKS: u32 = 0;
static mut STALL_DUMPED: bool = false;
/// Separate one-shot for the "kept while nothing is Ready" anomaly: with the
/// always-Ready ticker alive, `best == MAIN_INDEX` for a Normal current task
/// is impossible â€” 8 consecutive such ticks mean the Ready set is corrupted.
static mut NOREADY_TICKS: u32 = 0;
static mut NOREADY_DUMPED: bool = false;

fn new_task(entry: fn(), priority: u8, parent_id: u64) -> Task {
    unsafe {
        let stack: &'static mut [u8] = Box::leak(vec![0u8; STACK_SIZE].into_boxed_slice());
        let sp = prepare_stack(stack);
        let top = stack.as_ptr() as u64 + stack.len() as u64;
        let fpu_area = fpu::new_area();
        Task {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            entry,
            sp_slot: new_sp_slot(sp),
            stack,
            kstack_top: top & !0xF,
            state: State::Ready,
            // A task is born on the CPU that spawned it and stays there: every
            // task's kernel stack lives in that CPU's TSS/syscall slot only
            // while it runs there (kernel tasks have no thread migration).
            owner_cpu: smp::cpu_index() as u8,
            priority,
            slice_left: fresh_slice(priority),
            sleep_until_ms: 0,
            blocked_on_input: false,
            blocked_on_raw: false,
            wake_diag: false,
            parent_id,
            exit_code: 0,
            reaped: false,
            blocked_on_child: false,
            linux_abi: false,
            fpu_area,
        }
    }
}

pub fn spawn(entry: fn()) {
    {
        let _g = sched_guard();
        unsafe {
            TASKS.push(new_task(entry, PRIO_NORMAL, 0));
            TASK_COUNT += 1;
        }
    }
    // Another CPU may be idle: let it pick the new task up now.
    smp::kick_others();
}

/// Spawn a kernel task with an explicit priority (A3 gaming-track API).
pub fn spawn_prio(entry: fn(), priority: u8) {
    {
        let _g = sched_guard();
        unsafe {
            TASKS.push(new_task(entry, priority, 0));
            TASK_COUNT += 1;
        }
    }
    smp::kick_others();
}

/// B3: spawn a kernel task as a CHILD of the current task (used by the
/// lifecycle regression test so it can waitpid/kill it). Returns the new
/// task's id.
pub fn spawn_with_parent(entry: fn(), priority: u8, parent_id: u64) -> u64 {
    let id = {
        let _g = sched_guard();
        unsafe {
            let t = new_task(entry, priority, parent_id);
            let id = t.id;
            TASKS.push(t);
            TASK_COUNT += 1;
            id
        }
    };
    smp::kick_others();
    id
}

/// Spawn a ring-3 task. `user_rip` is the entry point of the user program,
/// whose pages must already be mapped (see `userspace::init`). The task's
/// first scheduling slot enters `user_entry_trampoline`, which iretqs to
/// CPL 3. Returns the new task's id. Parent is the current task (0 = main/
/// kernel when spawned at boot).
pub fn spawn_user(user_rip: u64, user_rsp: u64) -> u64 {
    let parent = current_task_id();
    spawn_user_with_parent(user_rip, user_rsp, parent)
}

/// B3: spawn_user with an explicit parent id (so a shell can waitpid its
/// children). Returns the new task's id.
pub fn spawn_user_with_parent(user_rip: u64, user_rsp: u64, parent_id: u64) -> u64 {
    let id = {
        let _g = sched_guard();
        unsafe {
            let stack: &'static mut [u8] = Box::leak(vec![0u8; STACK_SIZE].into_boxed_slice());
            let sp = prepare_stack_user(stack, user_rip, user_rsp);
            let top = stack.as_ptr() as u64 + stack.len() as u64;
            let fpu_area = fpu::new_area();
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            TASKS.push(Task {
                id,
                entry: bug_entry,
                sp_slot: new_sp_slot(sp),
                stack,
                kstack_top: top & !0xF,
                state: State::Ready,
                owner_cpu: smp::cpu_index() as u8,
                priority: PRIO_NORMAL,
                slice_left: fresh_slice(PRIO_NORMAL),
                sleep_until_ms: 0,
                blocked_on_input: false,
                blocked_on_raw: false,
                wake_diag: false,
                parent_id,
                exit_code: 0,
                reaped: false,
                blocked_on_child: false,
                linux_abi: false,
                fpu_area,
            });
            TASK_COUNT += 1;
            crate::serial_writeln!(
                "scheduler: user task spawned id={id} ({} tasks total), rip={user_rip:#x} rsp={user_rsp:#x}",
                TASK_COUNT
            );
            id
        }
    };
    smp::kick_others();
    id
}

/// M9.7: mark task `id` as a Linux-ABI binary (Linux syscall numbers +
/// conventions). Idempotent; silently ignores unknown ids.
pub fn set_linux_abi(id: u64) {
    let _g = sched_guard();
    unsafe {
        for i in 0..TASK_COUNT {
            if TASKS[i].id == id {
                TASKS[i].linux_abi = true;
                return;
            }
        }
    }
}

/// M9.7: does the currently running task (on THIS CPU) use the Linux ABI?
pub fn current_is_linux() -> bool {
    let _g = sched_guard();
    unsafe {
        let cur = cur_index();
        cur != MAIN_INDEX && TASKS[cur].linux_abi
    }
}

/// Placeholder entry for user tasks â€” they must enter via the ring-3
/// trampoline instead; landing here means the scheduler was misconfigured.
fn bug_entry() {
    crate::serial_writeln!("BUG: user task entered kernel task_entry");
    loop {
        x86_64::instructions::hlt();
    }
}

/// Mark the currently running task dead (kernel-task exit path). The task
/// keeps its stacks (it becomes a zombie the scheduler skips); kernel-owned
/// zombies (parent 0) are auto-reaped on the next wake pass.
pub fn mark_current_dead() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Dead;
            TASKS[cur].exit_code = 0;
            TASKS[cur].reaped = false;
        }
    });
}

/// M9.8: spawn a kernel task owned by CPU `cpu` (its `PerCpu` slot index).
///
/// The task is only ever scheduled on that CPU (see the affinity filter in
/// `plan_switch`), which is what lets the APs run real kernel work without
/// making the kernel's file-system/page-table/device layers SMP-safe yet.
/// Returns the new task's id.
pub fn spawn_on_cpu(entry: fn(), priority: u8, cpu: usize) -> u64 {
    let id = {
        let _g = sched_guard();
        unsafe {
            let mut t = new_task(entry, priority, 0);
            t.owner_cpu = cpu as u8;
            let id = t.id;
            TASKS.push(t);
            TASK_COUNT += 1;
            id
        }
    };
    // Wake the target CPU so it picks the task up immediately.
    let p = smp::per_cpu_ptr(cpu);
    if cpu != smp::cpu_index() && unsafe { (*p).online.load(Ordering::Relaxed) } {
        smp::send_ipi(
            unsafe { (*p).lapic_id.load(Ordering::Relaxed) },
            crate::apic::RESCHED_VECTOR,
        );
    }
    smp::kick_others();
    id
}

/// Is at least one task Ready and owned by this CPU right now?
///
/// Used by the CPU-idle bookkeeping (`bringup_task` checks it before it
/// declares bring-up done) and by anything that wants to know whether this
/// core has work without taking the scheduler lock for a pick.
pub fn has_ready_tasks() -> bool {
    let _g = sched_guard();
    let me = smp::cpu_index();
    unsafe {
        for i in 0..TASK_COUNT {
            if TASKS[i].state == State::Ready && usize::from(TASKS[i].owner_cpu) == me {
                return true;
            }
        }
    }
    false
}

/// B3: record a user task's exit status (SYS_EXIT / self-SYS_KILL), mark it
/// Dead and become a reap-able zombie. The scheduler wakes any blocked parent
/// on the next wake pass; the status persists until a parent reaps it.
/// IF-safe: also callable from task context (kernel `exit_current_code`).
pub fn record_exit(code: u32) {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Dead;
            TASKS[cur].exit_code = code;
            TASKS[cur].reaped = false;
            crate::serial_writeln!(
                "[exit] id={} code={} parent={}",
                TASKS[cur].id,
                code,
                TASKS[cur].parent_id
            );
        }
    });
}

/// B3: kill a task by id (SYS_KILL). Returns true if a live task was found;
/// its exit code is set to 137 and it becomes a reap-able zombie. Killing the
/// currently-running task is handled only via the syscall path (frame rewrite),
/// never through this function. IF-safe: callable from task context.
pub fn kill(pid: u64) -> bool {
    let mut victim_cpu = None;
    let killed = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        for i in 0..TASK_COUNT {
            if TASKS[i].id == pid && i != cur_index() && TASKS[i].state != State::Dead {
                // The owner is only meaningful while the task is Running, so
                // capture it BEFORE marking it Dead.
                victim_cpu = Some(TASKS[i].owner_cpu as usize);
                TASKS[i].state = State::Dead;
                TASKS[i].exit_code = 137;
                TASKS[i].reaped = false;
                return true;
            }
        }
        false
    });
    if killed {
        if let Some(cpu) = victim_cpu {
            if cpu != smp::cpu_index() && unsafe { (*smp::per_cpu_ptr(cpu)).online.load(Ordering::Relaxed) } {
                smp::send_ipi(
                    unsafe { (*smp::per_cpu_ptr(cpu)).lapic_id.load(Ordering::Relaxed) },
                    crate::apic::RESCHED_VECTOR,
                );
            }
        }
        smp::kick_others();
    }
    killed
}

/// Find a task by id, returning its index (None if absent/gone).
///
/// # Safety
/// The scheduler lock must be held (all callers hold it): the returned index
/// is only meaningful while `TASKS` cannot change underneath.
unsafe fn find_task(pid: u64) -> Option<usize> {
    for i in 0..TASK_COUNT {
        if TASKS[i].id == pid {
            return Some(i);
        }
    }
    None
}

/// B3: is there a wait-relevant child for the current task? Children that are
/// dead AND reaped are done; everything else (live or dead-unreaped) counts.
/// `pid == 0` matches any child. IF-safe: callable from task context.
pub fn child_waitable(pid: u64) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur == MAIN_INDEX {
            return false;
        }
        let cur_id = TASKS[cur].id;
        for i in 0..TASK_COUNT {
            if TASKS[i].parent_id == cur_id && (pid == 0 || TASKS[i].id == pid) {
                if TASKS[i].state != State::Dead || !TASKS[i].reaped {
                    return true;
                }
            }
        }
        false
    })
}

/// B3: reap a dead child of the current task. `pid == 0` reaps any child.
/// Returns (child_id, exit_code) and marks the zombie reaped.
/// IF-safe: callable from task context.
pub fn try_reap(pid: u64) -> Option<(u64, u32)> {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur == MAIN_INDEX {
            return None;
        }
        let cur_id = TASKS[cur].id;
        for i in 0..TASK_COUNT {
            if TASKS[i].parent_id == cur_id && (pid == 0 || TASKS[i].id == pid) {
                if TASKS[i].state == State::Dead && !TASKS[i].reaped {
                    let got = (TASKS[i].id, TASKS[i].exit_code);
                    TASKS[i].reaped = true;
                    return Some(got);
                }
            }
        }
        None
    })
}

/// B3: block the current task until one of its children dies (waitpid).
/// IF-safe: callable from task context.
pub fn block_on_child() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Blocked;
            TASKS[cur].blocked_on_child = true;
        }
    });
}

/// B3: the id of the current task's parent (0 if none).
pub fn current_parent_id() -> u64 {
    let _g = sched_guard();
    unsafe {
        let cur = cur_index();
        if cur == MAIN_INDEX {
            0
        } else {
            TASKS[cur].parent_id
        }
    }
}

/// Park the current task until LAPIC-time `wake_ms`, then yield the CPU.
/// Used by SYS_SLEEP (and kernel test tasks). `preempt()` wakes it when its
/// deadline passes. IF-safe (syscall/timer/task context).
pub fn sleep_current(wake_ms: u64) {
    let _g = sched_guard();
    unsafe {
        let cur = cur_index();
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Sleeping;
            TASKS[cur].sleep_until_ms = wake_ms;
            TASKS[cur].blocked_on_input = false;
            TASKS[cur].blocked_on_raw = false;
        }
    }
}

/// Kernel-task sleep helper: park `ms` milliseconds (rounds to the 1 ms tick)
/// and resume with interrupts enabled. Safe from a normal kernel task.
pub fn sleep_kernel(ms: u64) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let wake = crate::apic::ms_since_boot() + ms;
        sleep_current(wake);
        preempt();
    });
    x86_64::instructions::interrupts::enable();
}

/// M10b 6 (event-driven present): wake a Sleeping task immediately, before its
/// timer deadline.
///
/// This is the primitive that lets the present path be driven by DAMAGE rather
/// than by a poll timer. `sleep_kernel` parks a task and the 1 ms timer wakes it
/// when `now >= sleep_until_ms`; the flusher's response to a drawn change was
/// therefore bounded below by "how long until the timer tick, and then how long
/// until the scheduler runs me again" — measured as ~170 ms of scheduling
/// latency. When the console draws something, this makes the flusher Ready at
/// once, so it does not have to wait out its remaining back-off.
///
/// Safe to call from any context (IRQ or task): it takes the scheduler guard
/// and only ever promotes Sleeping -> Ready, which is idempotent and cannot
/// corrupt a Running/Dead task. Returns true if it actually woke someone.
pub fn wake_task_now(id: u64) -> bool {
    let mut woke = false;
    {
        let _g = sched_guard();
        unsafe {
            for t in TASKS.iter_mut() {
                if t.id == id {
                    if t.state == State::Sleeping {
                        t.state = State::Ready;
                        woke = true;
                    }
                    break;
                }
            }
        }
    }
    // A state change must reach the OTHER cpus: without the reschedule IPI
    // the newly-Ready task sits on the run queue until some cpu's next 1 ms
    // timer tick happens to look at it - which is precisely the ~105 ms
    // residual wake latency this was meant to remove. Every other state change
    // in this file (spawn/exit/kill/set_priority) kicks for the same reason.
    if woke {
        smp::kick_others();
    }
    woke
}
pub fn block_current_on_input() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Blocked;
            TASKS[cur].blocked_on_input = true;
        }
    });
}

/// B4: block the current task until a raw input event is pending (SYS_INPUT_READ
/// blocking mode). `preempt()`'s wake pass resumes it when `input::pending()`
/// turns true. IF-safe: callable from task context.
pub fn block_current_on_raw_input() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Blocked;
            TASKS[cur].blocked_on_raw = true;
        }
    });
}

/// Mark the current task Ready and give up the CPU immediately (SYS_YIELD /
/// cooperative round-robin). Returns into the task when it is rescheduled.
pub fn yield_current() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = sched_guard();
        let cur = cur_index();
        if cur != MAIN_INDEX && TASKS[cur].state == State::Running {
            TASKS[cur].state = State::Ready;
        }
    });
    preempt();
}

/// Context switches performed on the CALLING CPU so far (M9.8-f). A test can
/// sample this before and after a piece of code to prove that preemptions
/// really happened while that code was running.
pub fn switches_this_cpu() -> u64 {
    unsafe { (*smp::this_cpu()).switches.load(Ordering::Relaxed) }
}

/// Park the CALLING context as this CPU's idle context and never return.
///
/// The boot context calls this once `kernel_main` is done: from then on it is
/// simply the BSP's idle thread — picked whenever no task is Ready, and
/// preempted the moment something becomes Ready (the LAPIC timer ticks while
/// this loop halts, so the core wakes on any interrupt).
pub fn idle_forever() -> ! {
    loop {
        x86_64::instructions::hlt();
        preempt();
    }
}

/// A kernel task exits: mark it dead and park forever (preempt skips it from
/// now on). Call from kernel-task context with interrupts enabled.
pub fn exit_current() -> ! {
    mark_current_dead();
    loop {
        x86_64::instructions::hlt();
    }
}

/// B3: a kernel task exits WITH a status code (the ring-0 SYS_EXIT
/// equivalent): records `code`, becomes a reap-able zombie its parent can
/// wait on, and parks forever. IF-safe internally; must be called with
/// interrupts enabled so the 1 ms timer can switch away from the zombie.
pub fn exit_current_code(code: u32) -> ! {
    record_exit(code);
    loop {
        x86_64::instructions::hlt();
    }
}

/// B3 test support: has zombie `pid` had its status consumed (reaped)? A pid
/// that no longer exists counts as reaped. IF-safe: callable from task context.
pub fn is_reaped(pid: u64) -> bool {
    let _g = sched_guard();
    unsafe { find_task(pid).map_or(true, |i| TASKS[i].reaped) }
}

/// Raw context switch between kernel tasks.
///
/// This function is `#[naked]`: it has NO compiler prologue, so on entry
/// `rsp` points directly at the return address into `preempt` (pushed by
/// the `call`). ABI (System V): `rdi` = `&old.sp`, `rsi` = `&new.sp`,
/// `rdx` = old task's FPU area, `rcx` = new task's FPU area.
///
/// FPU/SSE state: `fxsave [rdx]` captures the outgoing task's x87+XMM
/// registers into its state image, then `fxrstor [rcx]` loads the incoming
/// task's image. This runs BEFORE the stack switch and inside the
/// naked function, so no compiler-generated code can touch XMM between the
/// save and the switch (which would corrupt the saved image) or after the
/// restore (which would corrupt the incoming task's state).
///
/// M9.8-f: this is the *fallback* body, used only when the CPU has no XSAVE;
/// [`context_switch_xsave`] is what a modern CPU runs (it preserves AVX/YMM
/// state too). `preempt` selects the body once per switch from the boot-time
/// CPUID decision.
///
/// We push the six callee-saved registers, store `rsp` into the old task's
/// slot, load the new task's saved `rsp`, pop its registers and `ret`.
/// The `ret` lands either in a fresh task (`task_entry`, prepared by
/// `prepare_stack`) or back inside `preempt` on a previously-saved stack â€”
/// both are symmetric because the saved layout is exactly
/// (descending addresses): r15, r14, r13, r12, rbp, rbx, return-address.
///
/// NOTE: rflags are deliberately not saved here. Preemption always happens
/// inside an interrupt handler, which already saved flags in its `iret`
/// frame on the task's own stack; and a fresh task enables interrupts in
/// `task_entry`. The function is typed as returning so that the code after
/// the call in `preempt` (the resume path!) is not optimized away.
#[unsafe(naked)]
unsafe extern "sysv64" fn context_switch_fxsave(
    _old: *mut u64,
    _new: *const u64,
    _old_fpu: *mut fpu::FpuArea,
    _new_fpu: *const fpu::FpuArea,
) {
    core::arch::naked_asm!(
        "fxsave [rdx]", // save outgoing x87+XMM state (no XMM use before this)
        "fxrstor [rcx]", // load incoming x87+XMM state
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, [rsi]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}

/// XSAVE variant of [`context_switch_fxsave`] (M9.8-f): identical stack
/// handling, but the task state image is swapped with `xsave64`/`xrstor64`,
/// which covers x87, SSE **and** the AVX YMM registers (XCR0 bits 0..2). This
/// is what lets a task keep live `__m256i` values across a preemption.
///
/// `xsave64` (not `xsave`) because this is 64-bit mode: it saves the
/// full-width state components and matches the 64-byte-aligned
/// [`fpu::FpuArea`] (both instructions require 64-byte alignment, which the
/// wrapper type guarantees; `fxsave`/`fxrstor` only need 16).
///
/// ARGUMENT 5 (`r8` in sysv64) is the `XCR0` value: `xsave64`/`xrstor64` take
/// the state-component mask in **EDX:EAX** — with garbage there the switch
/// silently saved/restored a random *subset* of the state (observed on the
/// first SMP run: images whose header said "x87 only" while XMM/YMM were
/// live, i.e. exactly the AVX-corruption signature; `-smp 1` only worked by
/// luck of the leftover register contents). The mask must be the *programmed*
/// `XCR0`, loaded explicitly before EACH of the two instructions (EDX:EAX is
/// also the mask input for `xrstor64`). `rdx` is clobbered while building the
/// mask, so the outgoing-image pointer first moves to `r9` (caller-saved,
/// dead after use — the interrupted task's GP registers live in its IRQ
/// frame, and only the callee-saved set below crosses the switch).
#[unsafe(naked)]
unsafe extern "sysv64" fn context_switch_xsave(
    _old: *mut u64,
    _new: *const u64,
    _old_fpu: *mut fpu::FpuArea,
    _new_fpu: *const fpu::FpuArea,
    _xcr0: u64,
) {
    core::arch::naked_asm!(
        "mov r9, rdx", // old_fpu -> scratch (rdx becomes the mask high half)
        "mov eax, r8d", // EDX:EAX = XCR0 mask (low half; XCR0 < 2^32)
        "xor edx, edx",
        "xsave64 [r9]", // save outgoing x87+SSE+AVX state
        "mov eax, r8d", // same mask for the restore
        "xor edx, edx",
        "xrstor64 [rcx]", // load incoming x87+SSE+AVX state
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, [rsi]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}

/// Set to `true` to trace every context switch on serial (very noisy â€”
/// each line costs ~9 ms at 115200 baud, which starves a 100 Hz tick).
const SCHED_TRACE: bool = false;

/// B5 stall diagnostic: called on every keep-current tick for a non-MAIN
/// task. Two one-shot triggers dump the task table:
///  - a task kept >= 150 consecutive ticks (runaway RT / broken rotation),
///  - a NORMAL task kept while NO task is Ready (impossible while the
///    always-Ready ticker lives) for >= 8 ticks.
fn stall_tick(cur: usize, best: usize, best_prio: u32) {
    unsafe {
        // RT keeps are BY DESIGN (A3: the 300 ms burst must survive Normal
        // preemption) and the timer period varies per boot (closed-loop
        // calibration skew: interval 499160..1409511 observed), so no fixed
        // tick threshold can separate a legit RT burst from a real runaway.
        // Exclude RT from both counters entirely. Accepted gap: a truly hung
        // RT task starves Normal tasks but leaves them Ready, which the
        // nothing-ready trigger does not cover.
        if TASKS[cur].priority == PRIO_RT {
            STALL_TICKS = 0;
            NOREADY_TICKS = 0;
            return;
        }
        let nothing_ready = best == MAIN_INDEX;
        // M9.8-(e): "nothing Ready anywhere" is NOT a stall any more. With task
        // migration (and work stealing) every runnable task is claimed almost
        // immediately, so a CPU legitimately sees an empty Ready set whenever
        // the other tasks are Running elsewhere, Sleeping, or Blocked — e.g.
        // four busy AP workers plus a shell waiting for a key. The old trigger
        // (nothing Ready for 8 ticks) was meaningful only in the CPU-affinity
        // world, where a Ready task could not be picked by the wrong CPU.
        // What is still provably broken is a task the wake pass should have
        // made runnable and did not: a sleeper past its deadline, or a
        // line/raw-input waiter with events actually pending.
        let now_ms = crate::apic::ms_since_boot();
        let mut stuck = false;
        if nothing_ready {
            unsafe {
                for i in 0..TASK_COUNT {
                    let t = &TASKS[i];
                    let overdue = t.state == State::Sleeping && now_ms >= t.sleep_until_ms;
                    let input_ready = t.state == State::Blocked
                        && ((t.blocked_on_input && crate::keyboard::line_pending())
                            || (t.blocked_on_raw && crate::input::pending()));
                    if overdue || input_ready {
                        stuck = true;
                        break;
                    }
                }
            }
        }
        if stuck {
            NOREADY_TICKS += 1;
        } else {
            NOREADY_TICKS = 0;
        }
        STALL_TICKS += 1;
        let runaway = STALL_TICKS >= 150 && !nothing_ready && !STALL_DUMPED;
        let vanished = NOREADY_TICKS >= 8 && !NOREADY_DUMPED;
        if !runaway && !vanished {
            return;
        }
        if runaway {
            STALL_DUMPED = true;
        }
        if vanished {
            NOREADY_DUMPED = true;
        }
        let rt = TASKS[cur].priority == PRIO_RT;
        crate::serial_writeln!(
            "[stall] cur={} held={} ticks best_id={} best_prio={} rt={} why={}",
            cur,
            STALL_TICKS,
            if best == MAIN_INDEX { 0u64 } else { TASKS[best].id },
            if best == MAIN_INDEX { u32::MAX as u64 } else { best_prio as u64 },
            rt as u8,
            if vanished { "nothing-ready" } else { "runaway" },
        );
        for i in 0..TASK_COUNT {
            // st: 0=Running 1=Ready 2=Sleeping 3=Blocked 4=Dead
            crate::serial_writeln!(
                "[stall] i={} id={} st={} prio={} sl={} in={} raw={} ch={}",
                i,
                TASKS[i].id,
                TASKS[i].state as u32,
                TASKS[i].priority,
                TASKS[i].slice_left,
                TASKS[i].blocked_on_input as u8,
                TASKS[i].blocked_on_raw as u8,
                TASKS[i].blocked_on_child as u8,
            );
        }
    }
}

/// Reschedule this CPU. Called from the LAPIC-timer handler (each CPU has its
/// own), from the reschedule IPI handler, and from kernel/syscall context.
///
/// IF-safe: callable from task context (IF=1) as well as interrupt/syscall
/// context (IF=0). Without this, a timer preemption landing between the TASKS
/// scan and `context_switch` nests a second preempt and desynchronizes
/// current/sp — observed as a silent whole-kernel freeze (M9.6-B3 regression
/// run). Each task's IF state is preserved on its own kernel stack (the
/// closure's saved-flags slot travels with the frame), so nesting semantics
/// stay correct.
///
/// M9.8 structure: the pick runs under `SCHED_LOCK` (two CPUs can never claim
/// the same task), while the switch runs AFTER the lock is released — see the
/// module docs for the handoff protocol that makes that safe.
pub fn preempt() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let me = smp::cpu_index();
        let plan = {
            let _g = SCHED_LOCK.lock(); // IF=0: the holder cannot be preempted
            unsafe {
                // The task this CPU suspended at its last switch is eligible
                // again now (deferred half of the handoff).
                release_pending_prev();
                plan_switch(me)
            }
        };
        let Some(plan) = plan else { return };
        crate::perf::ctx_switch(); // B5: an actual switch, not a keep-current tick
        // SAFETY: both slot pointers are stable (a per-task heap slot and this
        // CPU's PerCpu block) and owned by this CPU; `plan` was produced under
        // the lock, so no other CPU can have claimed either context.
        // M9.8-f: count real switches on this CPU (test observability: proves
    // preemptions happened inside a measured window).
    unsafe {
        (*smp::this_cpu()).switches.fetch_add(1, Ordering::Relaxed);
    }
    // M9.8-f: pick the state-image instruction pair once per switch from the
    // boot-time CPUID decision (XSAVE covers x87+SSE+AVX, FXSAVE the fallback).
    if fpu::xsave_enabled() {
        // The XCR0 mask is a REAL instruction operand for xsave/xrstor (see
        // the naked fn's docs): passing the programmed value explicitly is
        // what makes the save/restore cover the full x87+SSE+AVX state.
        unsafe {
            context_switch_xsave(
                plan.old_sp,
                plan.new_sp,
                plan.old_fpu,
                plan.new_fpu,
                fpu::xcr0() as u64,
            )
        };
    } else {
        unsafe { context_switch_fxsave(plan.old_sp, plan.new_sp, plan.old_fpu, plan.new_fpu) };
    }
    });
}

/// The switch `plan_switch` decided on: stable RSP slots + FPU images for this
/// CPU's outgoing and incoming contexts.
struct SwitchPlan {
    old_sp: *mut u64,
    new_sp: *const u64,
    old_fpu: *mut fpu::FpuArea,
    new_fpu: *const fpu::FpuArea,
}

/// Release the task this CPU suspended at its last context switch.
///
/// The switch itself cannot run under `SCHED_LOCK` (that would hold the lock
/// while the incoming task runs user code), so the outgoing task is left
/// `Running` and owned by this CPU until this CPU schedules again — by then
/// the incoming task has definitely resumed and the outgoing RSP was saved
/// long ago. Only then does it go back to `Ready`, exactly where the
/// single-CPU code used to mark it (at switch time).
///
/// Caller holds `SCHED_LOCK` (IF=0).
unsafe fn release_pending_prev() {
    let prev = smp::pending_prev();
    smp::set_pending_prev(MAIN_INDEX);
    if prev == MAIN_INDEX || prev == smp::current_index() {
        return;
    }
    if TASKS[prev].state == State::Running
        && usize::from(TASKS[prev].owner_cpu) == smp::cpu_index()
    {
        TASKS[prev].state = State::Ready;
    }
}

/// Pick the next context for CPU `me` (caller holds `SCHED_LOCK`, IF=0).
/// Returns the switch to perform, or `None` to keep running what is running.
unsafe fn plan_switch(me: usize) -> Option<SwitchPlan> {
    let n = TASK_COUNT;
    if n == 0 {
        return None;
    }
    let cur = cur_index();
    let cur_is_main = cur == MAIN_INDEX;

    // 1. Wake due sleepers + input/child-blocked tasks.
    wake_state_locked();

    // 2. Decrement the current task's slice (1 ms LAPIC ticks).
    if !cur_is_main && TASKS[cur].state == State::Running {
        let mut s = TASKS[cur].slice_left;
        if s > 0 {
            s -= 1;
            TASKS[cur].slice_left = s;
        }
    }

    // 3. Find the best Ready task: highest priority; on ties prefer a task
    //    whose `owner_cpu` is this CPU (its state - page-table entries, cache,
    //    device buffers - was last touched here), and otherwise *steal* the
    //    first Ready task found in round-robin order after the current one.
    //
    //    M9.8-(d)/(e): the M9.8 CPU-affinity filter is gone. It existed because
    //    the kernel's global structures (frame allocator, input/keyboard/mouse
    //    rings, cursor state, the file-system stack) were only safe while every
    //    task stayed on the CPU that spawned it. Those structures are now
    //    serialized by the kernel-service lock (`ksl`) and per-subsystem
    //    mutexes, so a task may run anywhere. Two properties make the steal
    //    safe:
    //      * a task is only ever claimed Ready -> Running *inside* this
    //        function, under SCHED_LOCK, so two CPUs cannot pick the same task;
    //      * the state a migrated task needs travels with it: its kernel stack
    //        and RSP slot are heap-allocated per task, and this CPU re-points
    //        TSS.RSP0 / the syscall scratch slot at the incoming task below.
    //    Stealing is what makes `-smp 4` actually shorten wall-clock work: an
    //    idle CPU no longer sits out while another CPU's queue is full. The
    //    Ready set of a task table this small IS the run queue - the owner
    //    field is the local fast path, not a restriction.
    let mut best = MAIN_INDEX;
    let mut best_prio = u32::MAX;
    let mut best_local = false;
    let start = if cur_is_main { 0 } else { cur + 1 };
    for k in 0..n {
        let idx = (start + k) % n;
        if TASKS[idx].state != State::Ready {
            continue;
        }
        let p = TASKS[idx].priority as u32;
        let local = usize::from(TASKS[idx].owner_cpu) == me;
        // Strictly better priority wins; equal priority prefers the local
        // task, and a task we already chose can only be displaced by a local
        // one of the same priority.
        if p < best_prio || (p == best_prio && local && !best_local && best != MAIN_INDEX) {
            best_prio = p;
            best = idx;
            best_local = local;
        }
    }

    // 4. Decide: keep the current task running, or switch.
    let cur_running = !cur_is_main && TASKS[cur].state == State::Running;
    if !cur_is_main {
        let rt = TASKS[cur].priority == PRIO_RT;
        let cur_prio = TASKS[cur].priority as u32;
        let keep = cur_running
            && (best == MAIN_INDEX
                || rt
                || (TASKS[cur].slice_left > 0 && best_prio >= cur_prio));
        if keep {
            // Nothing more urgent (or still within our slice): no switch.
            // The B5 stall diagnostic assumes the boot CPU's always-Ready
            // ticker, so on an AP (whose only Ready work is its own worker
            // tasks) "nothing ready" is normal and must not trip it.
            if me == 0 {
                stall_tick(cur, best, best_prio);
            }
            return None;
        }
    } else if best == MAIN_INDEX {
        // Already idle and nothing ready: stay put.
        STALL_TICKS = 0;
        NOREADY_TICKS = 0;
        return None;
    }
    STALL_TICKS = 0;
    NOREADY_TICKS = 0;

    // `best == MAIN_INDEX` means "switch to this CPU's own idle context".
    let next = best;
    if next == cur {
        return None;
    }

    // The outgoing task keeps `Running` + this CPU's ownership until
    // `release_pending_prev` runs here; a task that blocked itself is already
    // Blocked/Sleeping and is never released.
    if cur_running {
        smp::set_pending_prev(cur);
    }

    // Incoming task: fresh slice, Running, owned by this CPU.
    if next != MAIN_INDEX {
        TASKS[next].slice_left = fresh_slice(TASKS[next].priority);
        TASKS[next].state = State::Running;
        TASKS[next].owner_cpu = me as u8;
    }

    // Ring-3 entry (LAPIC timer / syscall / IPI) must land on the incoming
    // task's own kernel stack. IF=0 with the lock held, so re-pointing this
    // CPU's TSS.RSP0 / KSTACK_TOP is race-free.
    let ktop = if next == MAIN_INDEX {
        smp::idle_kstack_top()
    } else {
        TASKS[next].kstack_top
    };
    crate::gdt::set_kernel_stack(x86_64::VirtAddr::new(ktop));
    crate::syscall::set_kernel_stack_top(ktop);

    let plan = SwitchPlan {
        old_sp: sp_slot(cur),
        new_sp: sp_slot(next) as *const u64,
        old_fpu: fpu_ptr(cur),
        new_fpu: fpu_ptr(next) as *const fpu::FpuArea,
    };
    if SCHED_TRACE
        || (crate::bench::TRACE.load(Ordering::Relaxed) == 1
            && (crate::bench::is_bench_worker(cur) || crate::bench::is_bench_worker(next)))
    {
        crate::serial_writeln!(
            "[psw] cpu={me} cur={cur} next={next} curstate={:?} nextstate={:?}",
            unsafe { TASKS[cur].state } as u32,
            unsafe { TASKS[next].state } as u32,
        );
    }
    smp::set_current_index(next);
    Some(plan)
}

/// Called at the top of every preempt (IF=0): transition tasks whose
/// condition is now satisfied â€” a sleep deadline passed, a keyboard line
/// arrived, or a child died â€” from Sleeping/Blocked back to Ready. Also
/// auto-reaps zombies whose parent is gone (kernel task / dead parent).
///
/// M9.8: the caller holds the scheduler lock (IF=0), so the pass sees a stable
/// table and no two CPUs can double-wake a task.
fn wake_state_locked() {
    unsafe {
        let now_ms = crate::apic::ms_since_boot();
        for i in 0..TASK_COUNT {
            match TASKS[i].state {
                State::Sleeping if now_ms >= TASKS[i].sleep_until_ms => {
                    TASKS[i].state = State::Ready;
                }
                State::Sleeping
                    if now_ms > TASKS[i].sleep_until_ms + 3000 && !TASKS[i].wake_diag =>
                {
                    // One-time bring-up diagnostic: this sleeper is >3 s overdue.
                    // If this prints with advancing `now`, the wake pass runs and
                    // the deadline was wrong; if `now` is frozen, tick_ms died.
                    TASKS[i].wake_diag = true;
                    crate::serial_writeln!(
                        "[wake] OVERDUE idx={} now={} until={}",
                        i,
                        now_ms,
                        TASKS[i].sleep_until_ms
                    );
                }
                State::Blocked if TASKS[i].blocked_on_input => {
                    if crate::keyboard::line_pending() {
                        TASKS[i].state = State::Ready;
                    }
                }
                State::Blocked if TASKS[i].blocked_on_raw => {
                    if crate::input::pending() {
                        TASKS[i].state = State::Ready;
                    }
                }
                State::Blocked if TASKS[i].blocked_on_child => {
                    if has_dead_unreaped_child(i) {
                        TASKS[i].state = State::Ready;
                    }
                }
                State::Dead if !TASKS[i].reaped => {
                    // Auto-reap a zombie whose parent is the kernel/main
                    // (never waits) or whose parent is itself dead.
                    let reap = if TASKS[i].parent_id == 0 {
                        true
                    } else {
                        match find_task(TASKS[i].parent_id) {
                            Some(pi) => TASKS[pi].state == State::Dead,
                            None => true,
                        }
                    };
                    if reap {
                        TASKS[i].reaped = true;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Does task index `i` have a dead child whose status it has NOT yet reaped?
fn has_dead_unreaped_child(i: usize) -> bool {
    unsafe {
        let parent = TASKS[i].id;
        for j in 0..TASK_COUNT {
            if TASKS[j].parent_id == parent && TASKS[j].state == State::Dead && !TASKS[j].reaped {
                return true;
            }
        }
        false
    }
}

/// Current task's id (0 for this CPU's idle context). Meaningful only while
/// the caller cannot migrate CPUs (IF=0/syscall context), which the guard
/// pins down.
pub fn current_task_id() -> u64 {
    let _g = sched_guard();
    unsafe {
        let cur = cur_index();
        if cur == MAIN_INDEX {
            0
        } else {
            TASKS[cur].id
        }
    }
}

/// Change a task's priority (by task id). Returns false for a bad id.
pub fn set_priority(task_id: u64, priority: u8) -> bool {
    let changed = {
        let _g = sched_guard();
        let mut hit = false;
        unsafe {
            for i in 0..TASK_COUNT {
                if TASKS[i].id == task_id && TASKS[i].state != State::Dead {
                    TASKS[i].priority = priority;
                    hit = true;
                    break;
                }
            }
        }
        hit
    };
    // Another CPU may be running a lower-priority task right now.
    if changed {
        smp::kick_others();
    }
    changed
}

/// Entry point of a fresh kernel task (the `ret` target `context_switch`
/// pops). Runs on the task's own kernel stack on whichever CPU picked it.
fn task_entry() -> ! {
    // The task table may be mutating on another CPU, so read `entry` under the
    // scheduler lock (IF=0, as an IRQ handler would).
    let entry = {
        let _g = sched_guard();
        let idx = cur_index();
        unsafe { TASKS[idx].entry }
    };
    crate::serial_writeln!("task {} entered", cur_index());
    x86_64::instructions::interrupts::enable();
    entry();
    loop {
        x86_64::instructions::hlt();
    }
}

/// Build the initial stack for a user task: `context_switch` rets into
/// `user_entry_trampoline` with r15 = the user program's entry rip.
fn prepare_stack_user(stack: &mut [u8], user_rip: u64, user_rsp: u64) -> u64 {
    let top = stack.as_ptr() as usize + stack.len();
    let top_aligned = (top & !0xF) - 8;
    let mut sp = top_aligned;
    let trampoline: fn() -> ! = user_entry_trampoline;
    unsafe {
        // Descending layout matches context_switch's pop order exactly.
        sp -= 8; (sp as *mut u64).write_unaligned(trampoline as usize as u64); // ret addr
        sp -= 8; (sp as *mut u64).write_unaligned(0); // rbx
        sp -= 8; (sp as *mut u64).write_unaligned(0); // rbp
        sp -= 8; (sp as *mut u64).write_unaligned(0); // r12
        sp -= 8; (sp as *mut u64).write_unaligned(0); // r13
        sp -= 8; (sp as *mut u64).write_unaligned(user_rsp); // r14 = user rsp
        sp -= 8; (sp as *mut u64).write_unaligned(user_rip); // r15 = user rip
    }
    sp as u64
}

/// First code a new user task runs (ring 0, on its own kernel stack, straight
/// out of `context_switch`). Reads the user entry rip from r15, builds an
/// iret frame and drops to ring 3 with interrupts enabled.
fn user_entry_trampoline() -> ! {
    let user_rip: u64;
    let user_rsp: u64;
    unsafe {
        core::arch::asm!(
            "mov {}, r15",
            "mov {}, r14",
            out(reg) user_rip,
            out(reg) user_rsp,
            options(nomem, nostack, preserves_flags)
        );
    }
    // M9.8: a fresh task bypasses `preempt`'s resume path, so the CPU's
    // pending-prev slot must be released here instead (it is the first thing
    // `preempt` would otherwise do).
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let _g = SCHED_LOCK.lock();
        release_pending_prev();
    });
    let sels = crate::gdt::selectors();
    let user_cs = u64::from(sels.user_code_selector.0) | 3;
    let user_ss = u64::from(sels.user_data_selector.0) | 3;
    crate::serial_writeln!("user: dropping to ring 3, entry={user_rip:#x} stack={user_rsp:#x}");
    unsafe {
        core::arch::asm!(
            // Build the frame with IF=0; userland starts with IF=1 (rflags).
            "cli",
            "push {ss}",
            "push {rsp}",
            "push {flags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) user_ss,
            rsp = in(reg) user_rsp,
            flags = in(reg) 0x202u64,
            cs = in(reg) user_cs,
            rip = in(reg) user_rip,
            options(noreturn),
        );
    }
}

/// Build the initial stack for a kernel task.
///
/// `context_switch` restores registers in this order (top -> bottom):
///   return-addr, rbx, rbp, r12, r13, r14, r15
/// So `prepare_stack` must pre-populate, descending from the top:
///   [ret_addr = task_entry]  <- popped by `ret`
///   [rbx = 0] [rbp = 0] [r12..r15 = 0]
/// (No rflags slot: the switch does not popfq; a fresh task enables
/// interrupts in `task_entry`.)
fn prepare_stack(stack: &mut [u8]) -> u64 {
    let top = stack.as_mut_ptr() as usize + stack.len();
    // Tasks enter via `ret` (no call push), so the final rsp at `task_entry`
    // entry must be â‰¡ 8 (mod 16) to satisfy the SysV ABI invariant.
    let top_aligned = (top & !0xF) - 8;
    let mut sp = top_aligned;
    let trampoline: fn() -> ! = task_entry;
    unsafe {
        // Descending layout matches context_switch's pop order exactly.
        sp -= 8; (sp as *mut u64).write_unaligned(trampoline as usize as u64); // ret addr -> task_entry
        sp -= 8; (sp as *mut u64).write_unaligned(0);                           // rbx
        sp -= 8; (sp as *mut u64).write_unaligned(0);                           // rbp
        sp -= 8; (sp as *mut u64).write_unaligned(0);                           // r12
        sp -= 8; (sp as *mut u64).write_unaligned(0);                           // r13
        sp -= 8; (sp as *mut u64).write_unaligned(0);                           // r14
        sp -= 8; (sp as *mut u64).write_unaligned(0);                           // r15
    }
    sp as u64
}

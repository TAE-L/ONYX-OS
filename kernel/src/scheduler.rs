//! M9.6-A3: preemptive priority scheduler for kernel + ring-3 tasks.
//!
//! Three priority classes (RT > Normal > Idle), explicit time slices, and
//! full task states: Running/Ready/Sleeping/Blocked/Dead. The LAPIC timer
//! (1 ms) drives `preempt()`: it wakes due sleepers + input-blocked tasks,
//! then runs the highest-priority Ready task — keeping the current task
//! when nothing more urgent is ready (fewer needless context switches).

use crate::fpu;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const STACK_SIZE: usize = 64 * 1024;
const MAIN_INDEX: usize = usize::MAX;

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
    sp: u64,
    stack: &'static mut [u8],
    /// Top of this task's kernel stack (16-aligned). Ring-3 interrupt/syscall
    /// entry lands here (TSS.RSP0 + syscall entry KSTACK_TOP).
    kstack_top: u64,
    state: State,
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
    /// M9.6-B3: parent task id (0 = kernel/main — never a child of it).
    parent_id: u64,
    /// M9.6-B3: exit status recorded when the task becomes Dead (SYS_EXIT code,
    /// 137 for SYS_KILL).
    exit_code: u32,
    /// M9.6-B3: zombie's exit status already consumed by a parent (reaped).
    reaped: bool,
    /// M9.6-B3: this task is Blocked waiting for a child to die (waitpid).
    blocked_on_child: bool,
    /// 512-byte FXSAVE image swapped by `context_switch` (FPU/SSE state).
    /// Raw pointer (never null): the task list is a `static mut` Vec touched
    /// only with IF=0, so no reference may be formed through the index.
    fpu_area: *mut fpu::FpuArea,
}

static mut TASKS: Vec<Task> = Vec::new();
static mut TASK_COUNT: usize = 0;
static CURRENT: AtomicUsize = AtomicUsize::new(MAIN_INDEX);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static mut MAIN_SP: u64 = 0;
/// The boot/idle context's FXSAVE image (never dies, like MAIN_SP).
static mut MAIN_FPU: fpu::FpuArea = fpu::FpuArea::zeroed();

/// B5 stall diagnostic: consecutive ticks the current task was KEPT although
/// it is not RT. A Normal task must lose the CPU within its 8-tick slice;
/// far beyond that means the pick found nothing Ready (state corruption /
/// mass starvation). One-shot dump of the task table to serial.
static mut STALL_TICKS: u32 = 0;
static mut STALL_DUMPED: bool = false;
/// Separate one-shot for the "kept while nothing is Ready" anomaly: with the
/// always-Ready ticker alive, `best == MAIN_INDEX` for a Normal current task
/// is impossible — 8 consecutive such ticks mean the Ready set is corrupted.
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
            sp,
            stack,
            kstack_top: top & !0xF,
            state: State::Ready,
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
            fpu_area,
        }
    }
}

pub fn spawn(entry: fn()) {
    unsafe {
        TASKS.push(new_task(entry, PRIO_NORMAL, 0));
        TASK_COUNT += 1;
    }
}

/// Spawn a kernel task with an explicit priority (A3 gaming-track API).
pub fn spawn_prio(entry: fn(), priority: u8) {
    unsafe {
        TASKS.push(new_task(entry, priority, 0));
        TASK_COUNT += 1;
    }
}

/// B3: spawn a kernel task as a CHILD of the current task (used by the
/// lifecycle regression test so it can waitpid/kill it). Returns the new
/// task's id.
pub fn spawn_with_parent(entry: fn(), priority: u8, parent_id: u64) -> u64 {
    unsafe {
        let t = new_task(entry, priority, parent_id);
        let id = t.id;
        TASKS.push(t);
        TASK_COUNT += 1;
        id
    }
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
    unsafe {
        let stack: &'static mut [u8] = Box::leak(vec![0u8; STACK_SIZE].into_boxed_slice());
        let sp = prepare_stack_user(stack, user_rip, user_rsp);
        let top = stack.as_ptr() as u64 + stack.len() as u64;
        let fpu_area = fpu::new_area();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        TASKS.push(Task {
            id,
            entry: bug_entry,
            sp,
            stack,
            kstack_top: top & !0xF,
            state: State::Ready,
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
            fpu_area,
        });
        TASK_COUNT += 1;
        crate::serial_writeln!(
            "scheduler: user task spawned id={id} ({} tasks total), rip={user_rip:#x} rsp={user_rsp:#x}",
            TASK_COUNT
        );
        id
    }
}

/// Placeholder entry for user tasks — they must enter via the ring-3
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
        let cur = CURRENT.load(Ordering::Relaxed);
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Dead;
            TASKS[cur].exit_code = 0;
            TASKS[cur].reaped = false;
        }
    });
}

/// B3: record a user task's exit status (SYS_EXIT / self-SYS_KILL), mark it
/// Dead and become a reap-able zombie. The scheduler wakes any blocked parent
/// on the next wake pass; the status persists until a parent reaps it.
/// IF-safe: also callable from task context (kernel `exit_current_code`).
pub fn record_exit(code: u32) {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cur = CURRENT.load(Ordering::Relaxed);
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
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        for i in 0..TASK_COUNT {
            if TASKS[i].id == pid
                && i != CURRENT.load(Ordering::Relaxed)
                && TASKS[i].state != State::Dead
            {
                TASKS[i].state = State::Dead;
                TASKS[i].exit_code = 137;
                TASKS[i].reaped = false;
                return true;
            }
        }
        false
    })
}

/// Find a task by id, returning its index (None if absent/gone).
fn find_task(pid: u64) -> Option<usize> {
    unsafe {
        for i in 0..TASK_COUNT {
            if TASKS[i].id == pid {
                return Some(i);
            }
        }
        None
    }
}

/// B3: is there a wait-relevant child for the current task? Children that are
/// dead AND reaped are done; everything else (live or dead-unreaped) counts.
/// `pid == 0` matches any child. IF-safe: callable from task context.
pub fn child_waitable(pid: u64) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cur = CURRENT.load(Ordering::Relaxed);
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
        let cur = CURRENT.load(Ordering::Relaxed);
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
        let cur = CURRENT.load(Ordering::Relaxed);
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Blocked;
            TASKS[cur].blocked_on_child = true;
        }
    });
}

/// B3: the id of the current task's parent (0 if none).
pub fn current_parent_id() -> u64 {
    unsafe {
        let cur = CURRENT.load(Ordering::Relaxed);
        if cur == MAIN_INDEX {
            0
        } else {
            TASKS[cur].parent_id
        }
    }
}

/// Park the current task until LAPIC-time `wake_ms`, then yield the CPU.
/// Used by SYS_SLEEP (and kernel test tasks). `preempt()` wakes it when its
/// deadline passes. IF=0 (syscall/timer context).
pub fn sleep_current(wake_ms: u64) {
    unsafe {
        let cur = CURRENT.load(Ordering::Relaxed);
        if cur != MAIN_INDEX {
            TASKS[cur].state = State::Sleeping;
            TASKS[cur].sleep_until_ms = wake_ms;
            TASKS[cur].blocked_on_input = false;
            TASKS[cur].blocked_on_raw = false;
        } else {
            return; // MAIN can't sleep
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

/// Block the current task until a keyboard line is available, then yield.
/// Used by the blocking SYS_READ(0) path. `preempt()` wakes it when
/// `keyboard::line_pending()` turns true. IF-safe: callable from task context.
pub fn block_current_on_input() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cur = CURRENT.load(Ordering::Relaxed);
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
        let cur = CURRENT.load(Ordering::Relaxed);
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
        let cur = CURRENT.load(Ordering::Relaxed);
        if cur != MAIN_INDEX && TASKS[cur].state == State::Running {
            TASKS[cur].state = State::Ready;
        }
    });
    preempt();
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
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        find_task(pid).map_or(true, |i| TASKS[i].reaped)
    })
}

/// Raw context switch between kernel tasks.
///
/// This function is `#[naked]`: it has NO compiler prologue, so on entry
/// `rsp` points directly at the return address into `preempt` (pushed by
/// the `call`). ABI (System V): `rdi` = `&old.sp`, `rsi` = `&new.sp`,
/// `rdx` = old task's FPU area, `rcx` = new task's FPU area.
///
/// FPU/SSE state: `fxsave [rdx]` captures the outgoing task's x87+XMM
/// registers into its 512-byte FXSAVE area, then `fxrstor [rcx]` loads the
/// incoming task's image. This runs BEFORE the stack switch and inside the
/// naked function, so no compiler-generated code can touch XMM between the
/// save and the switch (which would corrupt the saved image) or after the
/// restore (which would corrupt the incoming task's state). Both areas are
/// 16-aligned (`FpuArea`) as the instructions require.
///
/// We push the six callee-saved registers, store `rsp` into the old task's
/// slot, load the new task's saved `rsp`, pop its registers and `ret`.
/// The `ret` lands either in a fresh task (`task_entry`, prepared by
/// `prepare_stack`) or back inside `preempt` on a previously-saved stack —
/// both are symmetric because the saved layout is exactly
/// (descending addresses): r15, r14, r13, r12, rbp, rbx, return-address.
///
/// NOTE: rflags are deliberately not saved here. Preemption always happens
/// inside an interrupt handler, which already saved flags in its `iret`
/// frame on the task's own stack; and a fresh task enables interrupts in
/// `task_entry`. The function is typed as returning so that the code after
/// the call in `preempt` (the resume path!) is not optimized away.
#[unsafe(naked)]
unsafe extern "sysv64" fn context_switch(
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

/// Set to `true` to trace every context switch on serial (very noisy —
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
        if nothing_ready {
            NOREADY_TICKS += 1;
        } else {
            NOREADY_TICKS = 0;
        }
        STALL_TICKS += 1;
        let runaway = STALL_TICKS >= 150 && !STALL_DUMPED;
        let vanished = nothing_ready && NOREADY_TICKS >= 8 && !NOREADY_DUMPED;
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

pub fn preempt() {
    // IF-safe: callable from task context (IF=1) as well as timer/syscall
    // context (IF=0). Without this, a timer preemption landing between the
    // TASKS scan and `context_switch` nests a second preempt and
    // desynchronizes CURRENT/sp — observed as a silent whole-kernel freeze
    // (M9.6-B3 regression run). Each task's IF state is preserved on its own
    // kernel stack (the closure's saved-flags slot travels with the frame),
    // so nesting semantics stay correct.
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let n = TASK_COUNT;
        if n == 0 {
            return;
        }
        let cur = CURRENT.load(Ordering::Relaxed);
        let cur_is_main = cur == MAIN_INDEX;

        // 1. Wake due sleepers + input-blocked tasks.
        wake_state_locked();

        // 2. Decrement the current task's slice (1 ms LAPIC ticks).
        if !cur_is_main && TASKS[cur].state == State::Running {
            let mut s = TASKS[cur].slice_left;
            if s > 0 {
                s -= 1;
                TASKS[cur].slice_left = s;
            }
        }

        // 3. Find the best Ready task: highest priority; on ties, round-robin
        //    starting after the current task.
        let mut best = MAIN_INDEX;
        let mut best_prio = u32::MAX;
        let start = if cur_is_main { 0 } else { cur + 1 };
        for k in 0..n {
            let idx = (start + k) % n;
            if TASKS[idx].state != State::Ready {
                continue;
            }
            let p = TASKS[idx].priority as u32;
            if p < best_prio {
                best_prio = p;
                best = idx;
            }
        }

        // 4. Decide: keep the current task running, or switch.
        if !cur_is_main {
            let running = TASKS[cur].state == State::Running;
            let rt = TASKS[cur].priority == PRIO_RT;
            let cur_prio = TASKS[cur].priority as u32;
            let keep = running
                && (best == MAIN_INDEX
                    || rt
                    || (TASKS[cur].slice_left > 0 && best_prio >= cur_prio));
            if keep {
                // Nothing more urgent (or still within our slice): no switch.
                stall_tick(cur, best, best_prio);
                return;
            }
            // The current task is leaving: mark it Ready so it can be picked
            // again, unless a syscall already moved it (Sleeping/Blocked/Dead).
            if running {
                TASKS[cur].state = State::Ready;
            }
        } else if best == MAIN_INDEX {
            // Already idle and nothing ready: stay put.
            unsafe {
                STALL_TICKS = 0;
                NOREADY_TICKS = 0;
            }
            return;
        }
        unsafe {
            STALL_TICKS = 0;
            NOREADY_TICKS = 0;
        };

        let next = if best == MAIN_INDEX { MAIN_INDEX } else { best };
        if next == cur {
            return;
        }

        // Incoming task: grant a fresh slice and mark Running.
        if next != MAIN_INDEX {
            TASKS[next].slice_left = fresh_slice(TASKS[next].priority);
            TASKS[next].state = State::Running;
        }

        // Ring-3 entry (LAPIC timer / syscall) must land on the incoming
        // task's own kernel stack. Interrupts are off here (inside the timer
        // handler / syscall dispatch), so re-pointing TSS.RSP0 / KSTACK_TOP
        // is race-free.
        let ktop = if next == MAIN_INDEX {
            idle_kstack_top()
        } else {
            TASKS[next].kstack_top
        };
        crate::gdt::set_kernel_stack(x86_64::VirtAddr::new(ktop));
        crate::syscall::set_kernel_stack_top(ktop);

        let old_sp: *mut u64 = if cur == MAIN_INDEX {
            core::ptr::addr_of_mut!(MAIN_SP)
        } else {
            core::ptr::addr_of_mut!(TASKS[cur].sp)
        };
        let new_sp: *const u64 = if next == MAIN_INDEX {
            core::ptr::addr_of!(MAIN_SP)
        } else {
            core::ptr::addr_of!(TASKS[next].sp)
        };
        // FPU/SSE images: the naked switch fxsave's the outgoing task's
        // registers into its area and fxrstor's the incoming task's.
        let old_fpu: *mut fpu::FpuArea = if cur == MAIN_INDEX {
            core::ptr::addr_of_mut!(MAIN_FPU)
        } else {
            TASKS[cur].fpu_area
        };
        let new_fpu: *const fpu::FpuArea = if next == MAIN_INDEX {
            core::ptr::addr_of!(MAIN_FPU)
        } else {
            TASKS[next].fpu_area
        };
        if SCHED_TRACE {
            crate::serial_writeln!(
                "preempt: cur={} next={} old={:#x} new={:#x}",
                cur, next, *old_sp, *new_sp
            );
        }
        CURRENT.store(next, Ordering::Relaxed);
        crate::perf::ctx_switch(); // B5: an actual switch, not a keep-current tick
        context_switch(old_sp, new_sp, old_fpu, new_fpu);
        if SCHED_TRACE {
            crate::serial_writeln!("preempt: returned (task resumed)");
        }
    });
}

/// Called at the top of every preempt (IF=0): transition tasks whose
/// condition is now satisfied — a sleep deadline passed, a keyboard line
/// arrived, or a child died — from Sleeping/Blocked back to Ready. Also
/// auto-reaps zombies whose parent is gone (kernel task / dead parent).
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

/// Current task's id (0 for the boot/idle context). Meaningful only in
/// syscall/exception context (IF=0), where the current index cannot change.
pub fn current_task_id() -> u64 {
    unsafe {
        let cur = CURRENT.load(Ordering::Relaxed);
        if cur == MAIN_INDEX {
            0
        } else {
            TASKS[cur].id
        }
    }
}

/// Change a task's priority (by task id). Returns false for a bad id.
pub fn set_priority(task_id: u64, priority: u8) -> bool {
    unsafe {
        for i in 0..TASK_COUNT {
            if TASKS[i].id == task_id && TASKS[i].state != State::Dead {
                TASKS[i].priority = priority;
                return true;
            }
        }
        false
    }
}

fn task_entry() -> ! {
    let idx = CURRENT.load(Ordering::Relaxed);
    let entry = unsafe { TASKS[idx].entry };
    crate::serial_writeln!("task {} entered", idx);
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

/// A dedicated (small) stack for the main/idle context so TSS.RSP0 and the
/// syscall KSTACK_TOP always point at a valid region even while the
/// scheduler idles in `kernel_main` (main never enters ring 3, so this is
/// just an invariant-keeping dummy).
static IDLE_KSTACK: spin::Lazy<[u8; 4096]> = spin::Lazy::new(|| [0; 4096]);

pub(crate) fn idle_kstack_top() -> u64 {
    let s = &*IDLE_KSTACK;
    s.as_ptr() as u64 + s.len() as u64
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
    // entry must be ≡ 8 (mod 16) to satisfy the SysV ABI invariant.
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

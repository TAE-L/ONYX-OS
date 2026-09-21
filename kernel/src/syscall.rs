//! M4: `syscall`/`sysret` fast path for ring-3 programs.
//!
//! ABI (System-V-like, mirroring Linux on x86-64):
//!   rax = syscall number, rdi/rsi/rdx = args 1-3, result in rax.
//!   rcx/r11 are clobbered by the CPU (user rip / rflags).
//!
//! Numbers:
//!   0 = SYS_WRITE(fd, buf, len) — serial console (fd 1/2) or file write
//!   1 = SYS_EXIT(code)          — mark the task dead; never returns to it
//!   2 = SYS_OPEN(path, len)     — fd for a path (create-if-missing)
//!   3 = SYS_READ(fd, buf, len)  — read from an fd at its position
//!   4 = SYS_CLOSE(fd)           — release a file fd
//!   5 = SYS_LS(path, len)       — print a directory listing to serial
//!   6 = SYS_MKDIR(path, len)    — create a directory (parent must exist)
//!   7 = SYS_SPAWN(path, len)    — load the ELF at `path` from the VFS and
//!                                 spawn it as a ring-3 task (region-checked)
//!   SYS_READ(0)                 — fd 0 is console input: one completed
//!                                 keyboard line (M8 shell line discipline)
//!   8 = SYS_GETTIME(clock)      — ns on the monotonic (0) / wall (1) clock
//!   9/10/11 = GETPID / SLEEP(ns) / YIELD
//!   12/13 = LSPCI / FLUSH (sync)
//!  14 = SYS_WAITPID(pid)        — (child_pid << 32) | exit_code; -ECHILD err
//!  15 = SYS_KILL(pid)           — terminate pid (exit code 137); self-kill
//!                                 reuses the SYS_EXIT frame rewrite
//!  16 = SYS_INPUT_READ(buf,len,blocking) — raw input events (24-byte
//!                                 records, see the `input` module)
//!  16 = SYS_INPUT_READ(buf,len,blocking) — raw input events (24-byte
//!                                 records, see the `input` module)
//!  17 = SYS_PERF()              — print the performance snapshot
//!  18 = SYS_STAT(path,len,out)  — 16-byte size/is_dir record into `out`
//!  19 = SYS_SEEK(fd,off,whence) — move a file fd's cursor (SET/CUR/END)
//!  20 = SYS_UNLINK(path,len)    — remove the file at `path`
//!  21 = SYS_RENAME(f,fl,t,tl)   — rename `from` to `to` (4th arg in r8)
//!
//! Entry stub mechanics: SYSCALL clears IF (SFMASK), parks the user RSP in a
//! static scratch slot, switches to the current task's kernel stack, saves
//! all user registers in a `SyscallFrame`, and calls `dispatch`. Returning
//! from `dispatch` pops the frame back and `sysretq`s to userland. SYS_EXIT
//! rewrites the frame so `sysretq` lands on a ring-3 `jmp $` trampoline; the
//! scheduler simply never picks the (dead) task again.

use core::arch::naked_asm;
use x86_64::registers::model_specific::{Efer, LStar, Msr, SFMask, Star};
use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use alloc::string::{String, ToString};

use crate::{blkcache, errno, gdt, input, keyboard, memory, scheduler, serial, serial_writeln, time, userspace, vfs};

/// SYS_WRITE(fd, buf, len): fd 1/2 = serial console, fd >= 3 = file write.
pub const SYS_WRITE: u64 = 0;
/// SYS_EXIT(code): mark the calling task dead; never returns to its code.
pub const SYS_EXIT: u64 = 1;
/// SYS_OPEN(path, len): open (create-if-missing) the file at `path` -> fd.
pub const SYS_OPEN: u64 = 2;
/// SYS_READ(fd, buf, len): read from a file fd at its position.
pub const SYS_READ: u64 = 3;
/// SYS_CLOSE(fd): release a file fd.
pub const SYS_CLOSE: u64 = 4;
/// SYS_LS(path, len): print the listing of `path` (both mounts) to serial;
/// len 0 lists the roots.
pub const SYS_LS: u64 = 5;
/// SYS_MKDIR(path, len): create the directory at `path` (parent must exist).
pub const SYS_MKDIR: u64 = 6;
/// SYS_SPAWN(path, len): load the ELF at `path` from the VFS and spawn it as
/// a ring-3 child of the caller (region-checked). Returns the child's pid
/// (> 0) on success, u64::MAX on error (never panics the kernel).
pub const SYS_SPAWN: u64 = 7;
/// SYS_GETTIME(clock): nanoseconds on the selected clock.
///   clock 0 = monotonic (TSC, since boot calibration)
///   clock 1 = wall clock (RTC anchor + monotonic; wraps at midnight)
/// Returns 0 if the clock is not calibrated yet, u64::MAX for a bad clock id.
pub const SYS_GETTIME: u64 = 8;
/// SYS_GETPID(): the calling task's id (0 for the boot/idle context).
pub const SYS_GETPID: u64 = 9;
/// SYS_SLEEP(ns): sleep at least `ns` nanoseconds (1 ms resolution), then
/// return. Blocks the task; safe because the syscall frame is self-contained.
pub const SYS_SLEEP: u64 = 10;
/// SYS_YIELD(): give up the CPU to a same-priority peer now.
pub const SYS_YIELD: u64 = 11;
/// SYS_LSPCI(): print the PCI device table (serial + framebuffer console).
pub const SYS_LSPCI: u64 = 12;
/// SYS_FLUSH(): invalidate the block cache (`sync`). Write-through, so no
/// data is pending on disk — this only drops cached sectors.
pub const SYS_FLUSH: u64 = 13;
/// SYS_WAITPID(pid): block until child `pid` (0 = any child) dies, then
/// return `(child_pid << 32) | exit_code`. u64::MAX (-ECHILD) when `pid` is
/// not a child of the calling task.
pub const SYS_WAITPID: u64 = 14;
/// SYS_KILL(pid): terminate task `pid` (it becomes a zombie with exit code
/// 137). Self-kill reuses the SYS_EXIT path. 0 on success, u64::MAX when
/// `pid` does not name a live task.
pub const SYS_KILL: u64 = 15;
/// SYS_INPUT_READ(buf, len, blocking): copy raw input events into `buf` as
/// fixed 24-byte little-endian records (see the `input` module). Returns the
/// byte count (a multiple of 24, at most len). When `blocking` is non-zero
/// and no event is pending, the task blocks until an IRQ pushes one (the
/// scheduler's wake pass resumes it); returns 0 immediately when `blocking`
/// is 0 and nothing is pending.
pub const SYS_INPUT_READ: u64 = 16;
/// SYS_PERF(): print the performance snapshot (counters, latency histograms,
/// cache hit rate, heap usage) on serial + framebuffer console.
pub const SYS_PERF: u64 = 17;
/// SYS_STAT(path, len, out): write a 16-byte `Stat` record to the user buffer
/// at `out` — u64 size (LE) at +0, u8 is_dir at +8 (7 pad bytes). -ENOENT /
/// -EINVAL / -EFAULT on errors.
pub const SYS_STAT: u64 = 18;
/// SYS_SEEK(fd, offset, whence): move a file fd's cursor. whence 0 = SET
/// (absolute), 1 = CUR (relative), 2 = END (relative to file size). Returns
/// the new absolute position; -EBADF/-EINVAL/-ENOENT on errors.
pub const SYS_SEEK: u64 = 19;
/// SYS_UNLINK(path, len): remove the file at `path`. -ENOENT if missing,
/// -EISDIR if `path` is a directory.
pub const SYS_UNLINK: u64 = 20;
/// SYS_RENAME(from, flen, to, tlen): rename the file `from` to `to`
/// (same directory). -ENOENT if `from` is missing, -EEXIST if `to` exists.
/// The 4th argument rides in r8 — `syscall` only clobbers rcx (RIP) and r11
/// (RFLAGS), so r8 survives into the saved frame untouched.
pub const SYS_RENAME: u64 = 21;

/// Scratch area for the syscall entry stub.
///
/// M9.8: the two hot slots (`kstack_top`, `user_rsp`) moved into the per-CPU
/// block (`smp::PerCpu`, reached as `gs:[32]`/`gs:[40]`): a single shared slot
/// would be clobbered by a second CPU entering a syscall at the same time —
/// including between the entry store and the load, which no `IF=0` protects
/// against across cores. `user_rsp` follows the classic pattern of being
/// re-read from the saved frame on the way out, so a task suspended inside a
/// syscall resumes with its own value.
///
/// What is left here is genuinely global: the `dispatch` function pointer.
/// The struct layout is kept (offsets 0/8/16 documented) because the stub
/// still loads `dispatch_ptr` RIP-relative to the `no_mangle` symbol — no
/// FSGSBASE (absent on QEMU's default CPU model, would raise #UD) and no `sym`
/// inline-asm operand (which LLVM's IAS rejects on this toolchain).
#[repr(C, align(16))]
struct SyscallScratch {
    /// Unused (per-CPU now; see `smp::GS_KSTACK_OFF`).
    kstack_top: u64,
    /// Unused (per-CPU now; see `smp::GS_USER_RSP_OFF`).
    user_rsp: u64,
    /// Function pointer to `dispatch`, read by the entry stub.
    dispatch_ptr: u64,
}
unsafe impl Sync for SyscallScratch {}
impl SyscallScratch {
    const fn new() -> Self {
        Self {
            kstack_top: 0,
            user_rsp: 0,
            dispatch_ptr: 0,
        }
    }
}
#[used]
#[no_mangle]
pub static mut SCRATCH: SyscallScratch = SyscallScratch::new();

/// Offset of the `user_rsp` field within `SyscallFrame` (from the frame base
/// the entry stub hands to `dispatch`).
const USER_RSP_OFF: u64 = 16;
/// Total frame size below the kernel-stack top (15 regs + 3 slots = 144).
/// The restore path reads `user_rsp` at `rsp(top) - (144 - USER_RSP_OFF)`.
const USER_RSP_RESTORE_OFF: u64 = 144 - USER_RSP_OFF;

/// Saved user state; layout matches the entry stub's push order exactly.
/// Frame base (= rsp after `sub rsp, 24`) is 16-byte aligned so that the
/// `call dispatch` presents a SysV-correct stack to the callee.
#[repr(C)]
pub struct SyscallFrame {
    _pad0: u64,
    _pad1: u64,
    /// User RSP at syscall time (filled in by the entry stub).
    pub user_rsp: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// Program the SYSCALL/SYSRET MSRs for the CURRENT CPU. Called once per CPU:
/// `init` on the boot CPU (after `gdt::init`), `init_ap` on each AP.
///
/// STAR/LSTAR/SFMASK are per-CPU MSRs — an AP running user code without them
/// would `syscall` straight into address 0 (M9.8).
fn program_msrs() {
    let sels = gdt::selectors();
    let kernel_cs = u64::from(sels.code_selector.0) & !3;
    let user_code = u64::from(sels.user_code_selector.0) & !3;
    let user_data = u64::from(sels.user_data_selector.0) & !3;
    // SYSRET derives SS = STAR[63:48]+8 and CS = STAR[63:48]+16, so the user
    // data selector must sit exactly 8 below the code selector (gdt.rs).
    assert_eq!(user_code - user_data, 8, "GDT: user data/code not adjacent");
    let star = ((user_code - 16) << 48) | (kernel_cs << 32);

    let entry_addr = syscall_entry as *const () as u64;
    msr_write(Star::MSR, star);
    msr_write(LStar::MSR, entry_addr);
    // Clear TF/IF/DF/NT/IOPL/AC/VIP/VIF on entry (Linux's proven mask);
    // notably IF=0 gives the kernel an interrupt-free entry window.
    msr_write(SFMask::MSR, 0x257FD5);

    // EFER.SCE (bit 0): enable the `syscall` instruction.
    let mut efer = Efer::MSR;
    let raw = unsafe { efer.read() };
    unsafe { efer.write(raw | 1) };

    // The dispatch pointer is global; the per-CPU slots (`gs:[32]`/`gs:[40]`)
    // are filled by `smp::init_bsp`/`smp::start_aps` and re-pointed by the
    // scheduler on every context switch.
    unsafe {
        SCRATCH.dispatch_ptr = dispatch as *const () as u64;
    }

    serial_writeln!("syscall: STAR={star:#x} LSTAR={entry_addr:#x} SFMASK=0x257fd5 EFER.SCE=1");
}

/// Program the SYSCALL MSRs on the boot CPU (after `gdt::init`).
pub fn init() {
    program_msrs();
    // Idle kernel stack for this CPU's syscall entry, until the scheduler
    // points it at the current task's stack.
    set_kernel_stack_top(crate::smp::idle_kstack_top());
}

/// Program the SYSCALL MSRs on an AP (from `smp::ap_entry`, once the AP's
/// GDT/TSS and per-CPU block are live).
pub fn init_ap() {
    program_msrs();
    set_kernel_stack_top(crate::smp::idle_kstack_top());
}

fn msr_write(msr: Msr, value: u64) {
    let mut m = msr;
    unsafe { m.write(value) };
}

/// Update the kernel-stack top in the per-CPU scratch area. The syscall entry
/// stub reads this via RIP-relative addressing, so the scheduler must call
/// this on every context switch (with IF off, which the switch guarantees).
/// Update the CURRENT CPU's kernel-stack top (the `gs:[32]` slot read by the
/// syscall entry stub). The scheduler calls this on every context switch (IF
/// off, which the switch guarantees), so each CPU enters the next syscall on
/// the stack of the task IT is about to run.
pub fn set_kernel_stack_top(top: u64) {
    crate::smp::set_kstack_top(top);
}

/// SYSCALL entry. On arrival: RSP = user RSP (untouched by `syscall`),
/// RCX = user RIP, R11 = user RFLAGS, IF = 0 (SFMASK). All user state is
/// captured in a `SyscallFrame`, `dispatch` runs, then everything is restored
/// and `sysretq` jumps back to ring 3.
///
/// Naked (no prologue) so RSP is still the user RSP on entry. M9.8: the
/// kernel-stack top and the parked user RSP live in the CALLING CPU's block
/// (`gs:[32]`, `gs:[40]` — see `smp::PerCpu`); `dispatch` is a plain global
/// read via RIP-relative addressing to the `no_mangle` symbol.
#[no_mangle]
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        // Park the true user RSP in this CPU's slot, then switch to the
        // current task's kernel stack from this CPU's slot. SFMASK cleared IF
        // on entry, so no interrupt can run between the two — and the slots
        // are private to this CPU, so a concurrent syscall on another core
        // cannot clobber them.
        "mov qword ptr gs:[40], rsp",
        "mov rsp, qword ptr gs:[32]",
        // Save all user registers (frame layout matches `SyscallFrame`).
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9", "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",
        // Frame padding for 16-byte alignment (kstack_top is 16-aligned).
        "sub rsp, 24",
        // Store the true user_rsp at frame offset 16.
        "mov rax, qword ptr gs:[40]",
        "mov [rsp + 16], rax",
        // dispatch(&mut frame) — global function pointer in SCRATCH.
        "mov rdi, rsp",
        "mov rax, [rip + SCRATCH + 16]",
        "call rax",
        // Restore registers.
        "add rsp, 24",
        "pop rax", "pop rbx", "pop rcx", "pop rdx",
        "pop rsi", "pop rdi", "pop rbp", "pop r8",
        "pop r9", "pop r10", "pop r11", "pop r12",
        "pop r13", "pop r14", "pop r15",
        // Back to the user stack: the true user RSP lives in THIS frame at
        // [frame+16] = [rsp_after_pops - 128] — reading it from the frame
        // (not a shared slot) makes a suspended syscall resume safely even if
        // other tasks ran their own syscalls meanwhile.
        "mov rsp, [rsp - 128]",
        "sysretq",
    );
}

/// Syscall dispatcher. Runs with IF=0 on the current task's kernel stack.
#[no_mangle]
extern "C" fn dispatch(f: &mut SyscallFrame) {
    let t0 = crate::perf::syscall_enter(f.rax);
    // M9.7: Linux-ABI binaries speak Linux syscall numbers (a wholly
    // different table — e.g. Linux write=1 collides with our SYS_EXIT=1),
    // pass arg 4 in r10, and return -errno. Route them to the shim first.
    if scheduler::current_is_linux() {
        linux_dispatch(f);
        crate::perf::syscall_exit(t0);
        return;
    }
    match f.rax {
        SYS_WRITE => f.rax = sys_write(f.rdi, f.rsi, f.rdx),
        SYS_EXIT => sys_exit(f),
        SYS_OPEN => f.rax = sys_open(f.rdi, f.rsi),
        SYS_READ => f.rax = sys_read(f.rdi, f.rsi, f.rdx),
        SYS_CLOSE => f.rax = sys_close(f.rdi),
        SYS_LS => f.rax = sys_ls(f.rdi, f.rsi),
        SYS_MKDIR => f.rax = sys_mkdir(f.rdi, f.rsi),
        SYS_SPAWN => f.rax = sys_spawn(f.rdi, f.rsi),
        SYS_GETTIME => f.rax = sys_gettime(f.rdi),
        SYS_GETPID => f.rax = scheduler::current_task_id(),
        SYS_SLEEP => f.rax = sys_sleep(f.rdi),
        SYS_YIELD => f.rax = sys_yield(),
        SYS_LSPCI => f.rax = sys_lspci(),
        SYS_FLUSH => {
            blkcache::flush();
            f.rax = 0;
        }
        SYS_WAITPID => f.rax = sys_waitpid(f.rdi),
        SYS_KILL => sys_kill(f),
        SYS_INPUT_READ => f.rax = sys_input_read(f.rdi, f.rsi, f.rdx),
        SYS_PERF => f.rax = sys_perf(),
        SYS_STAT => f.rax = sys_stat(f.rdi, f.rsi, f.rdx),
        SYS_SEEK => f.rax = sys_seek(f.rdi, f.rsi, f.rdx),
        SYS_UNLINK => f.rax = sys_unlink(f.rdi, f.rsi),
        SYS_RENAME => f.rax = sys_rename(f.rdi, f.rsi, f.rdx, f.r8),
        _ => f.rax = errno::err(errno::ENOSYS), // unknown syscall number
    }
    crate::perf::syscall_exit(t0);
}

/// write(fd, buf, len): fd 1/2 = serial console; fd >= 3 = file at fd pos.
fn sys_write(fd: u64, buf: u64, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    let len = (len as usize).min(4096);
    // Kernel reads the user buffer; validate it (presence + user mapping,
    // NOT writability) before touching it. A bad pointer is rejected, never
    // dereferenced.
    if !user_buf_ok(buf, len as u64, false) {
        return errno::err(errno::EFAULT);
    }
    if fd == 1 || fd == 2 {
        let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
        let n = serial::write_bytes(bytes);
        // Mirror terminal output onto the framebuffer text console.
        crate::framebuffer::console_bytes(bytes);
        return n as u64;
    }
    if fd >= 3 {
        let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
        return fd_write_at(fd as usize, bytes);
    }
    errno::err(errno::EBADF) // fd 0 is read-only (console input)
}

/// exit(code): the task becomes a zombie with `code` recorded. We cannot
/// sysret back into a dead task, so point rcx at the ring-3 `jmp $` trampoline;
/// the scheduler skips dead tasks from now on and a parent's SYS_WAITPID
/// reaps the status.
fn sys_exit(f: &mut SyscallFrame) {
    serial_writeln!("user exited cleanly, code={}", f.rdi);
    drop_task_fds(); // fds die with the task (before its id goes stale)
    // M9.7: Linux-ABI brk/mmap bookkeeping dies with the task too.
    userspace::linux_mem_drop(scheduler::current_task_id());
    scheduler::record_exit(f.rdi as u32);
    dead_frame_rewrite(f);
}

/// Rewrite the syscall frame so `sysretq` lands on the ring-3 `jmp $`
/// trampoline instead of the dead task's code (used by SYS_EXIT and
/// self-SYS_KILL). Never returns to the caller.
fn dead_frame_rewrite(f: &mut SyscallFrame) {
    f.rcx = userspace::USER_TRAMPOLINE;
    f.r11 = 0x202; // IF=1 so the zombie stays preemptible until skipped
}

/// waitpid(pid): block until a child dies, then return its (pid, exit code).
/// pid == 0 waits for any child. u64::MAX when `pid` is not a child of this
/// task. Blocks mid-syscall (the frame is self-contained, so this is safe).
fn sys_waitpid(pid: u64) -> u64 {
    loop {
        if let Some((cpid, code)) = scheduler::try_reap(pid) {
            return (cpid << 32) | u64::from(code);
        }
        if scheduler::child_waitable(pid) {
            scheduler::block_on_child();
            scheduler::preempt();
        } else {
            return errno::err(errno::ECHILD);
        }
    }
}

/// kill(pid): terminate a task. Self-kill reuses the exit path; otherwise
/// the target becomes a zombie with exit code 137 that its parent can reap.
fn sys_kill(f: &mut SyscallFrame) {
    let pid = f.rdi;
    if pid == scheduler::current_task_id() {
        drop_task_fds();
        scheduler::record_exit(137);
        dead_frame_rewrite(f);
        return;
    }
    if scheduler::kill(pid) {
        drop_fds_for(pid);
        f.rax = 0;
    } else {
        f.rax = errno::err(errno::ESRCH);
    }
}

/// Drop a specific task's fd table (SYS_KILL of another task).
fn drop_fds_for(pid: u64) {
    FDS.lock().retain(|t| t.id != pid);
}

/// input_read(buf, len, blocking): copy raw input events into `buf` as fixed
/// 24-byte little-endian records. Blocking form parks the task until the
/// scheduler's wake pass sees `input::pending()` (the frame is self-contained,
/// so a mid-syscall block is safe). Returns byte count (multiple of EV_SIZE,
/// at most len); 0 immediately when non-blocking and nothing pending; u64::MAX
/// for a bad buffer (validated with user_buf_ok) or a too-small buffer.
fn sys_input_read(buf: u64, len: u64, blocking: u64) -> u64 {
    let len = (len as usize).min(4096);
    if len < input::EV_SIZE {
        return errno::err(errno::EINVAL);
    }
    if !user_buf_ok(buf, len as u64, true) {
        return errno::err(errno::EFAULT);
    }
    loop {
        let ubuf = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
        let n = input::drain(ubuf);
        if n > 0 {
            return n as u64;
        }
        if blocking == 0 {
            return 0;
        }
        // Nothing pending: block until an IRQ pushes an event. The syscall
        // frame is self-contained, so we can safely resume after other tasks
        // ran their own syscalls meanwhile.
        scheduler::block_current_on_raw_input();
        scheduler::preempt();
    }
}

/// Called by the scheduler on every context switch (IF off) so that syscall
/// entry lands on the incoming task's own kernel stack on the switching CPU.
pub fn set_kstack(top: u64) {
    crate::smp::set_kstack_top(top);
}

// ---------------------------------------------------------------------------
// M6: file descriptors for the VFS. fds 1/2 are the serial console; file fds
// are handed out from 3 upward. Syscalls run with IF=0, so these short lock
// scopes are race-free.
// ---------------------------------------------------------------------------

const MAX_FDS: usize = 8;

/// An open file: its full VFS path and the read/write cursor position.
struct FdEntry {
    name: String,
    pos: u64,
}

/// M8: per-task fd tables. The shell and other user programs run
/// concurrently, and a single global table would let one task's open/close
/// steal another task's fd (observed: the shell's autoexec close(3) killed
/// fstest's open file). Keyed by scheduler task id; dropped on SYS_EXIT.
/// Syscalls run with IF=0, so these short lock scopes are race-free.
struct TaskFds {
    id: u64,
    fds: [Option<FdEntry>; MAX_FDS],
}

static FDS: spin::Mutex<alloc::vec::Vec<TaskFds>> = spin::Mutex::new(alloc::vec::Vec::new());

/// Run `f` with the current task's fd slot array, creating its table entry
/// on first use. `None` if allocation of a new table fails.
fn with_task_fds<T>(f: impl FnOnce(&mut [Option<FdEntry>; MAX_FDS]) -> T) -> Option<T> {
    let id = scheduler::current_task_id();
    let mut all = FDS.lock();
    if !all.iter().any(|t| t.id == id) {
        all.push(TaskFds {
            id,
            fds: [None, None, None, None, None, None, None, None],
        });
    }
    let t = all.iter_mut().find(|t| t.id == id)?;
    Some(f(&mut t.fds))
}

/// Drop the current task's fd table (SYS_EXIT: its fds die with it).
fn drop_task_fds() {
    let id = scheduler::current_task_id();
    FDS.lock().retain(|t| t.id != id);
}

fn with_fd<T>(fd: usize, f: impl FnOnce(&mut FdEntry) -> T) -> Option<T> {
    with_task_fds(|fds| fds.get_mut(fd).and_then(|o| o.as_mut()).map(f))?
}

/// Highest user virtual address (exclusive) for pointer validation. This is
/// NOT the ELF-segment bound (`userspace::USER_MAX_ADDR`, 256 MiB): user
/// *stacks* live at `0x4000_0000 + slot * 0x1000_0000`, so the bound must
/// cover them. 1 TiB does, while staying far below every kernel mapping
/// (heap at ~74 TiB, physical-memory window higher still) — and the real
/// gate is the page walk below: kernel pages lack USER_ACCESSIBLE and are
/// rejected by flags regardless of address.
const USER_PTR_MAX: u64 = 0x0000_1000_0000_0000; // 1 TiB

/// Validate a user-supplied buffer before the kernel touches it. Every byte
/// must lie in `[0x1000, USER_PTR_MAX)` (never the null page, never kernel
/// space) and every covered page must be mapped user-accessible — plus
/// user-*writable* when the kernel is going to write into it (SYS_READ).
/// Returns false on any violation; callers reject the syscall instead of
/// faulting (or worse, reading/writing kernel memory) from ring 0.
fn user_buf_ok(ptr: u64, len: u64, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    if ptr < 0x1000 || end > USER_PTR_MAX {
        return false;
    }
    // Walk the covered pages against the live page tables: a range check
    // alone would accept unmapped or kernel-only pages inside the (sparse)
    // user region. `memory::page_flags` yields Some only for present 4 KiB
    // mappings, so PRESENT is implied here.
    let first = ptr & !0xFFF;
    let last = (end - 1) & !0xFFF;
    let mut page = first;
    while page <= last {
        let Some(flags) = memory::page_flags(VirtAddr::new(page)) else {
            return false;
        };
        if !flags.contains(PageTableFlags::USER_ACCESSIBLE)
            || (write && !flags.contains(PageTableFlags::WRITABLE))
        {
            return false;
        }
        page += 0x1000;
    }
    true
}

/// Validate + normalize a user-supplied path: absolute, <= 128 bytes, no
/// `\`, no `.`/`..` components (empty components from `//` or a trailing
/// `/` are tolerated). Returns the path with its leading `/` kept. Syscalls
/// run with IF=0 on the user's own address space, so reading user memory is
/// safe here (same contract as sys_write).
fn user_path(ptr: u64, len: u64) -> Result<String, u64> {
    if len == 0 {
        return Err(errno::err(errno::ENOENT));
    }
    if len > 128 {
        return Err(errno::err(errno::ENAMETOOLONG));
    }
    if !user_buf_ok(ptr, len, false) {
        return Err(errno::err(errno::EFAULT));
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let path = match core::str::from_utf8(bytes) {
        Ok(p) => p.to_string(),
        Err(_) => return Err(errno::err(errno::EINVAL)),
    };
    if !path.starts_with('/') || path.contains('\\') {
        return Err(errno::err(errno::EINVAL));
    }
    for comp in path[1..].split('/') {
        if comp.is_empty() {
            continue;
        }
        if comp == "." || comp == ".." {
            return Err(errno::err(errno::EINVAL));
        }
    }
    Ok(path)
}

/// Map a VFS error to its errno-encoded syscall return (C2).
fn fs_err(e: vfs::FsError) -> u64 {
    match e {
        vfs::FsError::NotFound => errno::err(errno::ENOENT),
        vfs::FsError::Exists => errno::err(errno::EEXIST),
        vfs::FsError::NotSupported => errno::err(errno::EINVAL),
        vfs::FsError::NoSpace => errno::err(errno::ENOSPC),
        vfs::FsError::BadFs | vfs::FsError::Io(_) => errno::err(errno::EIO),
        vfs::FsError::IsDir => errno::err(errno::EISDIR),
    }
}

/// open(path, len): validate the path, confirm it exists / can be created,
/// and hand out a free fd >= 3. The fd stores the full VFS path.
fn sys_open(path: u64, len: u64) -> u64 {
    let path = match user_path(path, len) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Create-if-missing: a path not yet on disk creates a zero-length file
    // (SYS_WRITE on the returned fd then grows it; the parent directory must
    // already exist — mkdir makes those). Only NotFound is creatable;
    // Io/BadFs etc. surface as an error to the caller.
    if let Err(e) = vfs::open(&path) {
        let creatable = matches!(&e, vfs::FsError::NotFound)
            && vfs::write_at(&path, 0, &[]).is_ok();
        if !creatable {
            return fs_err(e);
        }
    }
    let fd = with_task_fds(|fds| {
        for i in 3..MAX_FDS {
            if fds[i].is_none() {
                fds[i] = Some(FdEntry { name: path.clone(), pos: 0 });
                return i as u64;
            }
        }
        errno::err(errno::EMFILE) // fd table full
    });
    match fd {
        Some(v) => v,
        None => errno::err(errno::ENOMEM),
    }
}

/// mkdir(path, len): create the directory at `path` (its parent must exist).
fn sys_mkdir(path: u64, len: u64) -> u64 {
    let path = match user_path(path, len) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match vfs::mkdir(&path) {
        Ok(()) => 0,
        Err(e) => fs_err(e),
    }
}

/// spawn(path, len): read the ELF at `path` from the VFS and spawn it as a
/// ring-3 child of the calling task. Returns the child's pid (> 0) on
/// success, u64::MAX on error (missing/invalid image, region conflict).
///
/// C1: the spawner passes its whole command line; the kernel tokenizes it —
/// argv[0] = the ELF path (whitespace-separated first token), remaining
/// tokens become argv[1..] — and builds the child's System V process-start
/// stack. Backward compatible: a bare path spawns with `argv = [path]`.
fn sys_spawn(path: u64, len: u64) -> u64 {
    let cmdline = match user_path(path, len) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let mut tokens = cmdline.split_whitespace();
    let Some(prog) = tokens.next() else {
        return u64::MAX;
    };
    let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    argv.push(prog);
    for t in tokens {
        argv.push(t);
    }
    match userspace::spawn_from_vfs_argv(prog, &argv) {
        Ok(pid) => pid,
        Err(e) => {
            serial_writeln!("sys_spawn({prog}): {e}");
            // C2: map the spawn-path failure strings to errnos.
            errno::err(match e {
                "no such file" | "empty file" => errno::ENOENT,
                "image too large" => errno::E2BIG,
                "region overlaps an already-loaded program" => errno::EEXIST,
                "runtime mapper unavailable" | "runtime frame allocator unavailable" => {
                    errno::ENOMEM
                }
                // every other message is an ELF-validation failure
                _ => errno::ENOEXEC,
            })
        }
    }
}

/// gettime(clock): nanoseconds on the selected clock (see SYS_GETTIME).
fn sys_gettime(clock: u64) -> u64 {
    match clock {
        0 => time::now_ns(),
        1 => time::realtime_ns(),
        _ => u64::MAX,
    }
}

/// sleep(ns): park the current task for at least `ns` nanoseconds (1 ms LAPIC
/// resolution). Yields mid-syscall; the scheduler wakes the task when its
/// deadline passes and the syscall simply returns 0.
fn sys_sleep(ns: u64) -> u64 {
    let wake_ms = crate::apic::ms_since_boot().saturating_add(ns.saturating_div(1_000_000).max(1));
    scheduler::sleep_current(wake_ms);
    scheduler::preempt();
    0
}

/// yield(): mark Ready + yield the CPU (cooperative round-robin). Returns 0
/// when the task is rescheduled.
fn sys_yield() -> u64 {
    scheduler::yield_current();
    0
}

/// lspci(): print the PCI device table on serial + framebuffer console.
fn sys_lspci() -> u64 {
    for line in crate::pci::render_lines() {
        serial_writeln!("{}", line);
        crate::framebuffer::console_bytes(line.as_bytes());
        crate::framebuffer::console_bytes(b"\n");
    }
    0
}

/// perf(): print the performance snapshot (counters, latency histograms,
/// cache hit rate, heap usage) on serial + framebuffer console.
fn sys_perf() -> u64 {
    crate::perf::print_snapshot();
    0
}

fn sys_read(fd: u64, buf: u64, len: u64) -> u64 {
    if len == 0 {
        return 0; // Linux: a zero-length read succeeds with 0
    }
    let len = (len as usize).min(4096);
    if fd == 0 {
        // fd 0 = console input: one completed keyboard line. Blocks until a
        // complete line is available instead of busy-polling (A3): with an
        // incomplete/empty line we park the task; the scheduler wakes it when
        // keyboard::line_pending() turns true.
        if !user_buf_ok(buf, len as u64, true) {
            return errno::err(errno::EFAULT);
        }
        let ubuf = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
        loop {
            // Drain any complete line that is already assembled.
            let n = keyboard::take_line(ubuf);
            if n > 0 {
                return n as u64;
            }
            if keyboard::line_pending() {
                // A complete line is coming but not fully drained yet: give
                // other tasks a chance to run (and the keyboard ring to fill).
                scheduler::yield_current();
            } else {
                // Nothing typed at all: block until a line completes.
                scheduler::block_current_on_input();
                scheduler::preempt();
            }
        }
    }
    // File fd: fd validity wins over buffer validity (Linux returns EBADF for
    // a bad fd even when the supplied buffer is itself unreadable/writable),
    // so resolve the fd before touching user memory.
    let (name, pos) = match with_fd(fd as usize, |e| (e.name.clone(), e.pos)) {
        Some(v) => v,
        None => return errno::err(errno::EBADF),
    };
    if !user_buf_ok(buf, len as u64, true) {
        return errno::err(errno::EFAULT);
    }
    let ubuf = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    match vfs::read_at(&name, pos, ubuf) {
        Ok(n) => {
            with_fd(fd as usize, |e| e.pos += n as u64);
            n as u64
        }
        Err(e) => {
            serial_writeln!("sys_read fd {fd} ({name}): {e:?}");
            errno::err(errno::EIO)
        }
    }
}

fn fd_write_at(fd: usize, buf: &[u8]) -> u64 {
    let (name, pos) = match with_fd(fd, |e| (e.name.clone(), e.pos)) {
        Some(v) => v,
        None => return errno::err(errno::EBADF),
    };
    match vfs::write_at(&name, pos, buf) {
        Ok(n) => {
            with_fd(fd, |e| e.pos += n as u64);
            n as u64
        }
        Err(e) => {
            serial_writeln!("sys_write fd {fd} ({name}): {e:?}");
            errno::err(errno::EIO)
        }
    }
}

fn sys_close(fd: u64) -> u64 {
    if fd < 3 || fd as usize >= MAX_FDS {
        return errno::err(errno::EBADF);
    }
    match with_task_fds(|fds| fds[fd as usize].take()) {
        Some(Some(_)) => 0,
        _ => errno::err(errno::EBADF),
    }
}

/// ls(path, len): print the listing of `path` on both mounted filesystems.
/// len == 0 lists the roots (compatible with the M6 no-arg form).
fn sys_ls(path: u64, len: u64) -> u64 {
    let target = if len == 0 {
        "/".to_string()
    } else {
        match user_path(path, len) {
            Ok(p) => p,
            Err(e) => return e,
        }
    };
    match vfs::list(&target) {
        Ok(entries) => {
            serial_writeln!("vfs: FAT32 {target} ({} entries)", entries.len());
            for e in &entries {
                serial_writeln!(
                    "vfs:   {} ({} bytes{})",
                    e.name,
                    e.size,
                    if e.is_dir { ", dir" } else { "" }
                );
            }
            // The secondary mount (ext2) is listed alongside, same path.
            match vfs::list2(&target) {
                Ok(entries) => {
                    serial_writeln!("vfs: ext2 {target} ({} entries)", entries.len());
                    for e in &entries {
                        serial_writeln!(
                            "vfs:   {} ({} bytes{})",
                            e.name,
                            e.size,
                            if e.is_dir { ", dir" } else { "" }
                        );
                    }
                }
                Err(e) => serial_writeln!("vfs: ext2 list {target}: {e:?}"),
            }
            0
        }
        Err(e) => {
            serial_writeln!("vfs: list {target} failed: {e:?}");
            errno::err(errno::ENOENT)
        }
    }
}

/// stat(path, len, out): write a 16-byte `Stat` record into the validated
/// user buffer at `out` (`u64 size` at +0 little-endian, `u8 is_dir` at +8).
fn sys_stat(path: u64, len: u64, out: u64) -> u64 {
    let path = match user_path(path, len) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !user_buf_ok(out, 16, true) {
        return errno::err(errno::EFAULT);
    }
    match vfs::stat(&path) {
        Ok(st) => {
            let buf = unsafe { core::slice::from_raw_parts_mut(out as *mut u8, 16) };
            buf[0..8].copy_from_slice(&st.size.to_le_bytes());
            buf[8] = u8::from(st.is_dir);
            buf[9..16].fill(0);
            0
        }
        Err(e) => fs_err(e),
    }
}

/// seek(fd, offset, whence): reposition a file fd's cursor. whence 0 = SET,
/// 1 = CUR, 2 = END (SEEK_END needs the file size, so it stats the path).
/// Returns the new absolute offset; bad fd → -EBADF, bad whence or a
/// negative result → -EINVAL.
fn sys_seek(fd: u64, offset: u64, whence: u64) -> u64 {
    if fd < 3 {
        return errno::err(errno::EBADF);
    }
    let off_i = offset as i64;
    let (name, cur) = match with_fd(fd as usize, |e| (e.name.clone(), e.pos)) {
        Some(v) => v,
        None => return errno::err(errno::EBADF),
    };
    let base: i64 = match whence {
        0 => 0,
        1 => cur as i64,
        2 => match vfs::stat(&name) {
            Ok(st) => st.size as i64,
            Err(e) => return fs_err(e),
        },
        _ => return errno::err(errno::EINVAL),
    };
    let new = match base.checked_add(off_i) {
        Some(n) if n >= 0 => n as u64,
        _ => return errno::err(errno::EINVAL),
    };
    with_fd(fd as usize, |e| e.pos = new);
    new
}

/// unlink(path, len): remove the file at `path` (directories → -EISDIR).
fn sys_unlink(path: u64, len: u64) -> u64 {
    let path = match user_path(path, len) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match vfs::unlink(&path) {
        Ok(()) => 0,
        Err(e) => fs_err(e),
    }
}

/// rename(from, flen, to, tlen): rename `from` to `to` (same directory).
fn sys_rename(from: u64, flen: u64, to: u64, tlen: u64) -> u64 {
    let from = match user_path(from, flen) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let to = match user_path(to, tlen) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match vfs::rename(&from, &to) {
        Ok(()) => 0,
        Err(e) => fs_err(e),
    }
}

// --- M9.7: Linux ABI shim ---------------------------------------------------
//
// Static Linux x86-64 binaries speak the Linux syscall table (via `syscall`):
//   rax = number, rdi/rsi/rdx/r10/r8/r9 = args 1-6, return in rax (-errno).
// Numbers are the real x86-64 Linux table — no renumbering. Coverage targets
// static-PIE startup and simple programs; unimplemented-but-harmless calls
// (futex without contention, sigaction, console ioctl) return success so
// glibc/musl startup paths proceed. fork/clone/execve/file-mmap: -ENOSYS.
mod lx {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const FSTAT: u64 = 5;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const WRITEV: u64 = 20;
    pub const ACCESS: u64 = 21;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const UNAME: u64 = 63;
    pub const GETCWD: u64 = 79;
    pub const GETPPID: u64 = 110;
    pub const ARCH_PRCTL: u64 = 158;
    pub const FUTEX: u64 = 202;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const EXIT_GROUP: u64 = 231;
    pub const OPENAT: u64 = 257;
    pub const GETRANDOM: u64 = 318;
    // arch_prctl codes
    pub const ARCH_SET_FS: u64 = 0x1002;
    pub const ARCH_GET_FS: u64 = 0x1003;
}

/// Linux `syscall` passes arg 4 in r10 (rcx is destroyed by the instruction).
/// The frame captures r10 verbatim, so the shim reads args 1-6 from rdi, rsi,
/// rdx, r10, r8, r9.
fn linux_dispatch(f: &mut SyscallFrame) {
    let pid = scheduler::current_task_id();
    match f.rax {
        lx::READ => f.rax = sys_read(f.rdi, f.rsi, f.rdx),
        lx::WRITE => f.rax = sys_write(f.rdi, f.rsi, f.rdx),
        lx::OPEN | lx::OPENAT => {
            // open(path,len) / openat(dirfd,path,flags,mode) — path in rsi.
            if f.rsi == 0 || f.rsi.checked_add(4096).is_none() {
                f.rax = errno::err(errno::EFAULT);
                return;
            }
            match linux_cstr_len(f.rsi) {
                Some(l) => f.rax = sys_open(f.rsi, l),
                None => f.rax = errno::err(errno::EFAULT),
            }
        }
        lx::CLOSE => f.rax = sys_close(f.rdi),
        lx::FSTAT => {
            // musl wants a valid struct; zero 144 bytes (x86-64 struct stat).
            match f.rsi.checked_add(144).filter(|_| linux_zero_user(f.rsi, 144)) {
                Some(_) => f.rax = 0,
                None => f.rax = errno::err(errno::EFAULT),
            }
        }
        lx::LSEEK => f.rax = sys_seek(f.rdi, f.rsi, f.rdx),
        lx::MMAP => {
            // Anonymous mmap only — the Linux ABI is
            //   mmap(addr, len, prot, flags, fd, off): rdi/rsi/rdx/r10/r8/r9,
            // with fd = -1 for MAP_ANONYMOUS. fd-backed mappings unsupported.
            let len = f.rsi;
            if f.r8.wrapping_add(1) != 0 {
                f.rax = errno::err(errno::ENOMEM); // fd-backed mmap unsupported
                return;
            }
            f.rax = match linux_mem_call(|m, fr| userspace::linux_mmap_anon(pid, len, m, fr)) {
                Some(r) => r,
                None => errno::err(errno::ENOMEM),
            };
        }
        lx::MUNMAP => f.rax = 0, // regions are never reclaimed back
        lx::BRK => {
            let addr = f.rdi;
            f.rax = match linux_mem_call(|m, fr| userspace::linux_brk(pid, addr, m, fr)) {
                Some(r) => r,
                None => errno::err(errno::ENOMEM),
            };
        }
        _ => linux_dispatch_more(f, pid),
    }
}

/// Second tier of the Linux shim (kept separate so each fn stays reviewable).
fn linux_dispatch_more(f: &mut SyscallFrame, pid: u64) {
    match f.rax {
        lx::RT_SIGACTION | lx::RT_SIGPROCMASK | lx::FUTEX | lx::IOCTL | lx::ACCESS => {
            // No real signals/access bits yet: report success so glibc/musl
            // startup proceeds (futex without waiters "wakes" nothing).
            f.rax = 0;
        }
        lx::SET_TID_ADDRESS => f.rax = pid,
        lx::PREAD64 => f.rax = sys_read(f.rdi, f.rsi, f.rdx), // offset ignored
        lx::WRITEV => {
            // iovec = { base: u64, len: u64 }; sum the per-vec write counts.
            let (iovs, cnt) = (f.rsi, f.rdx);
            let cnt = (cnt as usize).min(64);
            let mut total = 0u64;
            for i in 0..cnt {
                let rec = iovs.checked_add(i as u64 * 16).unwrap_or(u64::MAX);
                if !user_buf_ok(rec, 16, false) {
                    f.rax = errno::err(errno::EFAULT);
                    return;
                }
                let base = unsafe { (rec as *const u64).read_volatile() };
                let len = unsafe { ((rec as *const u64).add(1) as *const u64).read_volatile() };
                if len == 0 {
                    continue;
                }
                let r = sys_write(f.rdi, base, len);
                if r & 0x8000_0000_0000_0000 != 0 {
                    f.rax = r; // -errno from the first failed vec
                    return;
                }
                total += r;
            }
            f.rax = total;
        }
        lx::NANOSLEEP => {
            // struct timespec { tv_sec: u64, tv_nsec: u64 } at rdi.
            if user_buf_ok(f.rdi, 16, false) {
                let sec = unsafe { (f.rdi as *const u64).read_volatile() };
                let nsec = unsafe { ((f.rdi as *const u64).add(1)).read_volatile() };
                f.rax = sys_sleep(sec.saturating_mul(1_000_000_000).saturating_add(nsec));
            } else {
                f.rax = errno::err(errno::EFAULT);
            }
        }
        lx::GETPID => f.rax = pid,
        lx::GETPPID => f.rax = 0,
        _ => linux_dispatch_tail(f, pid),
    }
}

/// Third tier: uname/getcwd/TLS/time/exit/random + the helper fns.
fn linux_dispatch_tail(f: &mut SyscallFrame, pid: u64) {
    match f.rax {
        lx::UNAME => {
            // struct utsname: six 65-byte NUL-padded string fields.
            let buf = f.rdi;
            const SZ: u64 = 6 * 65;
            if user_buf_ok(buf, SZ, true) {
                unsafe {
                    core::ptr::write_bytes(buf as *mut u8, 0, SZ as usize);
                    let put = |off: u64, s: &str| {
                        for (i, b) in s.as_bytes().iter().enumerate() {
                            ((buf + off) as *mut u8).add(i).write_volatile(*b);
                        }
                    };
                    put(0, "OnyxOS");
                    put(65, "onyx");
                    put(130, "9.7.0");
                    put(195, "(none)");
                    put(260, "x86_64");
                    put(325, "(none)");
                }
                f.rax = 0;
            } else {
                f.rax = errno::err(errno::EFAULT);
            }
        }
        lx::GETCWD => {
            // Report "/" (buf at rdi, size rsi) — enough for shell-like tools.
            let (buf, sz) = (f.rdi, f.rsi);
            if sz >= 2 && user_buf_ok(buf, 2, true) {
                unsafe {
                    (buf as *mut u8).write_volatile(b'/');
                    ((buf as *mut u8).add(1)).write_volatile(0);
                }
                f.rax = 2;
            } else {
                f.rax = errno::err(errno::EINVAL);
            }
        }
        lx::ARCH_PRCTL => {
            // TLS base (FS) setup: required by static-PIE before it touches
            // any TCB-relative data. QEMU's default CPU lacks FSGSBASE, so go
            // through the IA32_FS_BASE MSR (wrmsr is ring-0 only — we are).
            let (code, addr) = (f.rdi, f.rsi);
            match code {
                lx::ARCH_SET_FS => {
                    if addr != 0 && !user_buf_ok(addr, 1, true) {
                        f.rax = errno::err(errno::EFAULT);
                        return;
                    }
                    let mut msr = Msr::new(0xC000_0100); // IA32_FS_BASE
                    unsafe { msr.write(addr) };
                    serial_writeln!("lx: ARCH_SET_FS base={:#x}", addr);
                    f.rax = 0;
                }
                lx::ARCH_GET_FS => {
                    if !user_buf_ok(addr, 8, true) {
                        f.rax = errno::err(errno::EFAULT);
                        return;
                    }
                    let v = unsafe { Msr::new(0xC000_0100).read() };
                    unsafe { (addr as *mut u64).write_volatile(v) };
                    f.rax = 0;
                }
                _ => f.rax = errno::err(errno::EINVAL),
            }
        }
        lx::CLOCK_GETTIME => {
            // clk 0 = REALTIME (wall), 1 = MONOTONIC (TSC ns) -> timespec out.
            let (clk, out) = (f.rdi, f.rsi);
            if !user_buf_ok(out, 16, true) {
                f.rax = errno::err(errno::EFAULT);
                return;
            }
            let ns = if clk == 0 { sys_gettime(1) } else { sys_gettime(0) };
            let ns = if ns == u64::MAX { 0 } else { ns };
            unsafe {
                (out as *mut u64).write_volatile(ns / 1_000_000_000);
                ((out as *mut u64).add(1)).write_volatile(ns % 1_000_000_000);
            }
            f.rax = 0;
        }
        lx::EXIT_GROUP => sys_exit(f),
        lx::GETRANDOM => {
            // Not cryptographic: a counter stream is enough for the programs
            // the shim serves (they seed their PRNGs from it).
            let (buf, len) = (f.rdi, f.rsi);
            let len = (len as usize).min(256);
            if len == 0 || !user_buf_ok(buf, len as u64, true) {
                f.rax = errno::err(errno::EFAULT);
                return;
            }
            static LX_RNG: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0x9E37_79B9_7F4A_7C15);
            let mut s = LX_RNG.load(core::sync::atomic::Ordering::Relaxed);
            unsafe {
                for i in 0..len {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    (buf as *mut u8).add(i).write_volatile(s as u8);
                }
            }
            LX_RNG.store(s, core::sync::atomic::Ordering::Relaxed);
            f.rax = len as u64;
        }
        _ => f.rax = errno::err(errno::ENOSYS),
    }
    let _ = pid;
}

/// Run `f(&mut mapper, &mut frames)` with the runtime paging + global frame
/// snapshot (syscall context is not the boot allocator). None when not ready.
fn linux_mem_call<R>(
    f: impl FnOnce(
        &mut x86_64::structures::paging::OffsetPageTable<'static>,
        &mut memory::BootInfoFrameAllocator,
    ) -> R,
) -> Option<R> {
    let mut mapper = memory::runtime_mapper()?;
    memory::with_global_frames(|frames| f(&mut mapper, frames))
}

/// User NUL-terminated string length, validating each byte's mapping first.
fn linux_cstr_len(mut p: u64) -> Option<u64> {
    let mut n = 0u64;
    loop {
        if n >= 4096 || memory::page_flags(VirtAddr::new(p)).is_none() {
            return None;
        }
        let b = unsafe { (p as *const u8).read_volatile() };
        if b == 0 {
            return Some(n);
        }
        n += 1;
        p += 1;
    }
}

/// Zero `len` user bytes with mapping validation per byte; false on a bad
/// range (nothing is written in that case).
fn linux_zero_user(start: u64, len: u64) -> bool {
    for i in 0..len {
        let p = start + i;
        if memory::page_flags(VirtAddr::new(p)).is_none() {
            return false;
        }
        unsafe { (p as *mut u8).write_volatile(0) };
    }
    true
}






//! M4: ring-3 userspace support — loads the embedded user ELF into a fresh
//! user address-space region and registers it with the scheduler.
//!
//! Reserved virtual layout (low half, all unmapped by the bootloader):
//!   0x003FF000   exit trampoline page (2 bytes: `jmp $`) — SYS_EXIT sysrets
//!                here so the CPU never returns into a dead task's code
//!   0x00400000   user program (ELF PT_LOAD segments, linked at 0x400000)
//!   0x40000000   user stack top (64 KiB mapped below it, grows down)
//!
//! The ELF64 header is parsed by hand (it is a tiny, stable format) to avoid
//! pulling in a third-party loader crate. Segments are mapped with
//! `USER_ACCESSIBLE`, which `Mapper::map_to` propagates to parent tables
//! automatically.

use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTableFlags, Size4KiB,
};
use x86_64::VirtAddr;

use alloc::vec::Vec;
use spin::Mutex;

use crate::{errno, memory, scheduler, serial_writeln, vfs};

/// Virtual extents (page-aligned [start, end)) of all loaded user programs.
/// Programs share one address space and are linked at distinct fixed bases
/// (hello @ 0x400000, fstest @ 0x800000, shell @ 0xC00000), so a program may
/// only spawn when its region does not overlap any loaded program's region.
/// (Per-process CR3 switching retires this in a later milestone.)
static REGIONS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());


/// Base of the user program (matches the user crate's `--image-base`).
pub const USER_TEXT_BASE: u64 = 0x40_0000;
/// Virtual address of the 2-byte exit trampoline (`jmp $`).
pub const USER_TRAMPOLINE: u64 = 0x3F_F000;
/// Number of user stack slots handed out so far (one per user task).
static USER_STACK_SLOTS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
/// Mapped user-stack size in pages (64 KiB per task).
const USER_STACK_PAGES: u64 = 16;
/// User stacks sit 256 MiB apart starting at 1 GiB — well clear of the
/// < 256 MiB user-image region, and each task owns its own slot.
fn user_stack_top(slot: usize) -> u64 {
    0x4000_0000 + slot as u64 * 0x1000_0000
}
/// Upper bound for any user segment (sanity guard against a bad ELF).
const USER_MAX_ADDR: u64 = 0x1000_0000; // 256 MiB

/// M9.7: ET_DYN (static-PIE) images state *preferred* vaddrs, not absolute
/// ones — the kernel maps them at this fixed load region and applies
/// R_X86_64_RELATIVE relocations (shared machinery with M12's PE relocations).
/// 32 MiB: clear of the fixed-base program images (4/8/12/16/20/24 MiB), the
/// Linux brk heap (64 MiB) and the stacks (1 GiB+).
const LNX_PIE_LOAD_BASE: u64 = 0x200_0000;

// M9.7 ELF facts (ELF64 header offsets in parens).
const ET_DYN: u16 = 3; // e_type (16)
const SHT_RELA: u32 = 4; // section header sh_type
const R_X86_64_RELATIVE: u64 = 8; // Elf64_Rela r_info low 32 bits

/// M4 demo program (ring-3 syscall smoke test), baked in via artifact deps
/// (cargo exposes CARGO_BIN_FILE_* to this crate's rustc directly).
pub static HELLO_ELF: &[u8] = include_bytes!(env!("CARGO_BIN_FILE_USER_HELLO_hello"));
/// M6 filesystem test program (ring-3 FAT32 access via syscalls).
pub static FSTEST_ELF: &[u8] = include_bytes!(env!("CARGO_BIN_FILE_USER_FSTEST_fstest"));

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1 << 0;
const PF_W: u32 = 1 << 1;

/// Map the user program, its stack and the exit trampoline, then register the
/// ring-3 task with the scheduler. Must run before interrupts are enabled.
pub fn init(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) {
    // 1. Exit trampoline: EB FE = `jmp $` (ring 3: executable, not writable).
    // Map writable for the write (CR0.WP=1 applies to supervisor too), then
    // tighten to exec-only.
    let tramp_page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(USER_TRAMPOLINE));
    map_page(
        mapper,
        frame_allocator,
        USER_TRAMPOLINE,
        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE,
    );
    unsafe {
        (USER_TRAMPOLINE as *mut u8).write_volatile(0xEB);
        ((USER_TRAMPOLINE + 1) as *mut u8).write_volatile(0xFE);
    }
    unsafe {
        mapper
            .update_flags(
                tramp_page,
                PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE,
            )
            .expect("failed to protect trampoline page")
            .flush();
    }

}

/// Page-aligned virtual extent of `elf`'s PT_LOAD segments, as *mapped*.
/// Defensive (no panics): `None` on any malformation — a disk-loaded ELF must
/// never take the kernel down in this pre-validation step.
///
/// M9.7: for ET_DYN (static-PIE) the file's vaddrs are preferred, so the
/// mapped extent is `LNX_PIE_LOAD_BASE + [0, extent)` (the kernel maps the
/// image at the fixed load region); for ET_EXEC the vaddrs are absolute.
fn elf_region(elf: &[u8]) -> Option<(u64, u64)> {
    if elf.len() < 64 || !elf.starts_with(b"\x7FELF") || elf[4] != 2 || elf[5] != 1 {
        return None;
    }
    let e_type = u16::from_le_bytes(elf.get(16..18)?.try_into().ok()?) as u64;
    let rd32 = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(elf.get(off..off + 4)?.try_into().ok()?))
    };
    let rd64 = |off: usize| -> Option<u64> {
        Some(u64::from_le_bytes(elf.get(off..off + 8)?.try_into().ok()?))
    };
    let phoff = rd64(32)? as usize;
    let phentsize = u16::from_le_bytes(elf.get(54..56)?.try_into().ok()?) as usize;
    let phnum = u16::from_le_bytes(elf.get(56..58)?.try_into().ok()?) as usize;
    if phentsize < 56 {
        return None;
    }
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    for i in 0..phnum {
        let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if rd32(ph)? != PT_LOAD {
            continue;
        }
        let vaddr = rd64(ph + 16)?;
        let memsz = rd64(ph + 40)?;
        let start = vaddr & !0xFFF;
        let end = vaddr.checked_add(memsz)?.checked_add(0xFFF)? & !0xFFF;
        lo = lo.min(start);
        hi = hi.max(end);
    }
    if lo == u64::MAX || hi <= lo {
        return None;
    }
    // M9.7: ET_DYN loads at the fixed PIE region — extents shift by the load
    // base. (lo is the page-rounded preferred base; every segment vaddr sits
    // at/above it, so the relative extent is simply hi - lo.)
    let (mlo, mhi) = if e_type as u16 == ET_DYN {
        (LNX_PIE_LOAD_BASE, LNX_PIE_LOAD_BASE + (hi - lo))
    } else {
        (lo, hi)
    };
    if mlo < USER_TEXT_BASE || mhi > USER_MAX_ADDR {
        return None;
    }
    Some((mlo, mhi))
}

/// Common spawn path (embedded and from-disk): reject overlapping regions,
/// map a fresh user-stack slot, load `elf`, register the ring-3 task.
///
/// `frames` is either the boot allocator (boot path, interrupts still off)
/// or the global runtime allocator (SYS_SPAWN path, IF=0 from SFMASK) — both
/// single-owner at their call sites, so no extra locking is needed here.
pub fn spawn_bytes(
    elf: &[u8],
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    argv: &[&str],
) -> Result<u64, &'static str> {
    let region = elf_region(elf).ok_or("not a valid ELF64 user program")?;
    {
        let regions = REGIONS.lock();
        if regions.iter().any(|(s, e)| region.0 < *e && *s < region.1) {
            return Err("region overlaps an already-loaded program");
        }
    }

    let slot = USER_STACK_SLOTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let top = user_stack_top(slot);
    let stack_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    for i in 0..USER_STACK_PAGES {
        map_page(mapper, frame_allocator, top - (i + 1) * 0x1000, stack_flags);
    }

    let info = load_elf(elf, mapper, frame_allocator)?;
    // C1: build the System V process-start stack (argc/argv/envp/auxv) in
    // the freshly mapped stack slot; the task enters with rsp -> argc.
    let rsp = build_initial_stack(top, argv, &info)?;
    REGIONS.lock().push(region);
    serial_writeln!(
        "userspace: ELF loaded, entry = {:#x}, stack top = {top:#x}, rsp = {rsp:#x}, region = {:#x}..{:#x}",
        info.entry,
        region.0,
        region.1
    );

    // The task drops to ring 3 on its first scheduling slot; its task id is
    // the child pid returned to the spawner (B3 waitpid parent tracking).
    let pid = scheduler::spawn_user(info.entry, rsp);
    // M9.7: a Linux-ABI binary gets the Linux syscall shim (Linux numbers,
    // arg 4 in r10, `-errno` returns). Detected via the ONYXLNX marker our
    // builder embeds or a static-glibc `GLIBC_2.` version string.
    if elf.windows(8).any(|w| w == b"ONYXLNX\0") || elf.windows(7).any(|w| w == b"GLIBC_2") {
        scheduler::set_linux_abi(pid);
        serial_writeln!("userspace: Linux-ABI ELF detected (M9.7 syscall shim active)");
    }
    Ok(pid)
}

/// Map a fresh user-stack slot, load `elf`, and register the task with the
/// scheduler. Call after `init()`, with interrupts still disabled.
pub fn spawn_embedded(
    elf: &'static [u8],
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    argv: &[&str],
) {
    if let Err(e) = spawn_bytes(elf, mapper, frame_allocator, argv) {
        serial_writeln!("userspace: embedded spawn failed: {e}");
    }
}

/// Spawn a byte slice via the runtime paging/frames snapshot (SYS_SPAWN
/// context; IF=0). Shared by `spawn_from_vfs` and the boot fallback path.
pub fn spawn_bytes_runtime_argv(elf: &[u8], argv: &[&str]) -> Result<u64, &'static str> {
    match memory::with_global_frames(|frame_allocator| {
        match memory::runtime_mapper() {
            Some(mut mapper) => spawn_bytes(elf, &mut mapper, frame_allocator, argv),
            None => Err("runtime mapper unavailable"),
        }
    }) {
        Some(res) => res,
        None => Err("runtime frame allocator unavailable"),
    }
}

/// M8: read the ELF at `path` from the VFS and spawn it as a ring-3 task
/// (child of the calling task). Runs in syscall context (IF=0) using the
/// runtime paging/frames snapshot in `memory`. Failure never panics: the
/// caller gets `Err` and keeps running. Success returns the child's task id.
/// C1: `argv[0]` is the command as typed; extra tokens flow to the child.
pub fn spawn_from_vfs_argv(path: &str, argv: &[&str]) -> Result<u64, &'static str> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut off = 0u64;
    loop {
        let n = vfs::read_at(path, off, &mut chunk).map_err(|e| match e {
            // C2: keep NotFound distinct so SYS_SPAWN can return ENOENT.
            vfs::FsError::NotFound => "no such file",
            _ => "VFS read failed",
        })?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        off += n as u64;
        if buf.len() > 8 * 1024 * 1024 {
            return Err("image too large");
        }
    }
    if buf.is_empty() {
        return Err("empty file");
    }
    serial_writeln!("userspace: {path}: read {} bytes from disk", buf.len());
    spawn_bytes_runtime_argv(&buf, argv)
}

/// Compat wrapper: spawn with `argv = [path]` (path-only command line).
pub fn spawn_from_vfs(path: &str) -> Result<u64, &'static str> {
    spawn_from_vfs_argv(path, &[path])
}

// --- M9.7: Linux-ABI memory (brk heap + anonymous mmap) ---------------------
//
// Linux binaries grow their heap with brk(2) and map anonymous memory with
// mmap(2). Both need page mapping in syscall context, so the handlers live
// here and run through the runtime paging/frames snapshot. Per-task cursors:
//   (pid, brk_top, mmap_top) — brk grows from LINUX_HEAP_BASE upward, mmap
// hands out fresh regions from LINUX_MMAP_BASE upward (never freed back;
// entries die with the task in `linux_mem_drop`).
//
// Layout (M9.7): program images occupy 4..20 MiB (hello/fstest/shell/evtest/
// argtest) plus the M9.7 Linux test programs (24 MiB fixed, 32 MiB PIE load
// region), so the Linux brk heap sits at 64 MiB and the mmap arena at
// 128..256 MiB — everything inside the 256 MiB reserved user region and well
// clear of the per-task stacks at 1 GiB + slot·256 MiB. A heap/mmap page that
// lands on an already-mapped page would panic `map_page` (the address space
// is shared until per-process CR3s exist), so these bases must stay clear of
// every program region forever.

/// Base of the Linux-ABI brk heap (64 MiB — above every program image).
pub const LINUX_HEAP_BASE: u64 = 0x400_0000;
/// Base of the Linux-ABI anonymous mmap region (128 MiB).
pub const LINUX_MMAP_BASE: u64 = 0x800_0000;
/// Upper bound of the Linux-ABI mmap arena (256 MiB = `USER_MAX_ADDR`).
const LINUX_MMAP_END: u64 = 0x1000_0000;

/// (pid, brk_top, mmap_top) per live Linux-ABI task.
static LINUX_MEM: Mutex<Vec<(u64, u64, u64)>> = Mutex::new(Vec::new());

/// Free a task's brk/mmap bookkeeping (SYS_EXIT / SYS_KILL path).
pub fn linux_mem_drop(pid: u64) {
    LINUX_MEM.lock().retain(|e| e.0 != pid);
}

fn linux_mem_entry(pid: u64) -> (u64, u64, u64) {
    let mut mem = LINUX_MEM.lock();
    if let Some(pos) = mem.iter().position(|e| e.0 == pid) {
        mem[pos]
    } else {
        mem.push((pid, LINUX_HEAP_BASE, LINUX_MMAP_BASE));
        *mem.last().expect("just pushed")
    }
}

fn linux_mem_set(pid: u64, brk: u64, mmap: u64) {
    let mut mem = LINUX_MEM.lock();
    if let Some(pos) = mem.iter().position(|e| e.0 == pid) {
        mem[pos] = (pid, brk, mmap);
    }
}

/// Map `pages` user RW/NX pages starting at the page-aligned `start`.
fn linux_map_pages(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    start: u64,
    pages: u64,
) {
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    for i in 0..pages {
        map_page(mapper, frame_allocator, start + i * 0x1000, flags);
    }
}

/// Linux brk(2): `addr == 0` queries the current break; otherwise grow the
/// heap to `addr` (mapping the new pages). On refusal (shrink / out of range)
/// returns the current break, like the Linux failure convention.
pub fn linux_brk(
    pid: u64,
    addr: u64,
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> u64 {
    let (_, brk, mmap) = linux_mem_entry(pid);
    if addr == 0 {
        return brk;
    }
    if addr <= brk || addr >= LINUX_MMAP_BASE {
        return brk; // no shrink; out-of-range grows are refused
    }
    let new_top = (addr + 0xFFF) & !0xFFF;
    let pages = (new_top - brk + 0xFFF) / 0x1000;
    linux_map_pages(mapper, frame_allocator, brk, pages);
    linux_mem_set(pid, new_top, mmap);
    new_top
}

/// Linux mmap(2) anonymous path: hand out a fresh page-aligned region of
/// `len` bytes. File-backed mappings (fd >= 0) are not supported (ENOMEM).
pub fn linux_mmap_anon(
    pid: u64,
    len: u64,
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> u64 {
    if len == 0 {
        return (errno::EINVAL as i64).wrapping_neg() as u64;
    }
    let (_, brk, mmap) = linux_mem_entry(pid);
    let pages = (len + 0xFFF) / 0x1000;
    let end = mmap + pages * 0x1000;
    if end > LINUX_MMAP_END {
        return (errno::ENOMEM as i64).wrapping_neg() as u64;
    }
    linux_map_pages(mapper, frame_allocator, mmap, pages);
    linux_mem_set(pid, brk, end);
    mmap
}

// --- C1: System V process-start stack --------------------------------------
//
// At ring-3 entry the ABI expects (System V AMD64, Linux layout):
//   [rsp]     argc
//   [rsp+8]   argv[0] .. argv[n-1], NULL
//   ...       envp[0] .. envp[m-1], NULL
//   ...       auxv (key,value) pairs, AT_NULL last
//   above     the argv/envp strings + 16 AT_RANDOM bytes

/// ELF facts the initial stack needs (returned by `load_elf`).
struct ElfLoadInfo {
    entry: u64,
    phnum: u64,
    /// Virtual address of the program-header table (AT_PHDR) when the phdr
    /// blob lies inside a mapped PT_LOAD segment; `None` otherwise.
    phdr_vaddr: Option<u64>,
    /// M9.7: AT_BASE — the image's load base for ET_DYN (static-PIE treats it
    /// as the program's own load address); 0 for fixed-base ET_EXEC.
    at_base: u64,
}

// auxv keys (Linux ABI subset).
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7; // M9.7: dynamic/static-PIE load base
const AT_ENTRY: u64 = 9;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// 16 AT_RANDOM bytes. Placeholder entropy (TSC through a splitmix-style
/// mixer): the ABI requires the pointer to exist, not cryptographic
/// quality — a real RNG lands with the entropy subsystem.
fn at_random_bytes() -> [u8; 16] {
    let mut state = unsafe { core::arch::x86_64::_rdtsc() };
    let mut out = [0u8; 16];
    for chunk in out.chunks_exact_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = state;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        chunk.copy_from_slice(&x.to_le_bytes());
    }
    out
}

/// Write `bytes` ending exactly at `*sp` (stack grows down: `sp` is left at
/// the first byte of the written region). Caller owns the address space.
fn write_bytes_down(sp: &mut u64, bytes: &[u8]) {
    unsafe {
        *sp -= bytes.len() as u64;
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), *sp as *mut u8, bytes.len());
    }
}

/// C1: build the process-start stack in the freshly mapped user-stack slot
/// and return the initial rsp (16-byte aligned, pointing at argc).
fn build_initial_stack(
    stack_top: u64,
    argv: &[&str],
    info: &ElfLoadInfo,
) -> Result<u64, &'static str> {
    // The kernel passes an empty environment today; C3's env work fills it.
    let envp: &[&str] = &[];

    // Size guard: strings + block must fit the mapped stack comfortably.
    let mut strings = 16usize; // AT_RANDOM bytes
    for s in argv {
        strings += s.len() + 1;
    }
    for s in envp {
        strings += s.len() + 1;
    }
    // argc + argv[] + NUL + envp[] + NUL + 9 auxv pairs (PHDR, PHENT, PHNUM,
    // PAGESZ, AT_BASE, ENTRY, AT_RANDOM, AT_EXECFN, AT_NULL) — the AT_NULL
    // pair must stay inside the budget: overflowing the block writes the
    // trailing zeros over the argv[0] string just above it.
    let block = 8 * (3 + argv.len() + envp.len()) + 8 * (2 * 9);
    if strings + block + 64 > (USER_STACK_PAGES * 0x1000) as usize {
        return Err("argv/envp too large for the user stack");
    }

    // Strings + random bytes, written downward from the 16-aligned top.
    let mut sp = stack_top & !0xF;

    let random = at_random_bytes();
    write_bytes_down(&mut sp, &random);
    let random_ptr = sp;

    let mut argv_ptrs: Vec<u64> = Vec::with_capacity(argv.len());
    for s in argv.iter().rev() {
        let mut bytes = Vec::with_capacity(s.len() + 1);
        bytes.extend_from_slice(s.as_bytes());
        bytes.push(0);
        write_bytes_down(&mut sp, &bytes);
        argv_ptrs.push(sp);
    }
    argv_ptrs.reverse(); // ascending: argv[0] .. argv[n-1]
    let execfn = argv_ptrs.first().copied().unwrap_or(0);

    let mut envp_ptrs: Vec<u64> = Vec::with_capacity(envp.len());
    for s in envp.iter().rev() {
        let mut bytes = Vec::with_capacity(s.len() + 1);
        bytes.extend_from_slice(s.as_bytes());
        bytes.push(0);
        write_bytes_down(&mut sp, &bytes);
        envp_ptrs.push(sp);
    }
    envp_ptrs.reverse();

    // auxv pairs, in the order a walker reads them (AT_NULL terminates).
    // auxv (key,value) pairs, AT_NULL last. Written explicitly, not from a
    // stack array: `[(u64,u64); 9]` (144 bytes) trips this toolchain's LLVM
    // backend with an "offset is not a multiple of 16" codegen error.
    let _ = block as u64; // (block layout still includes the full auxv block)

    // The contiguous process-start block (argc .. AT_NULL): starts
    // 16-aligned directly below the strings region.
    let block_start = (sp - block as u64) & !0xF;
    let mut w = block_start as *mut u64;
    unsafe {
        *w = argv.len() as u64; // argc
        w = w.add(1);
        for p in &argv_ptrs {
            *w = *p;
            w = w.add(1);
        }
        *w = 0; // argv terminator
        w = w.add(1);
        for p in &envp_ptrs {
            *w = *p;
            w = w.add(1);
        }
        *w = 0; // envp terminator
        w = w.add(1);
        macro_rules! auxv {
            ($k:expr, $v:expr) => {{
                *w = $k;
                w = w.add(1);
                *w = $v;
                w = w.add(1);
            }};
        }
        auxv!(AT_PHDR, info.phdr_vaddr.unwrap_or(0));
        auxv!(AT_PHENT, 56);
        auxv!(AT_PHNUM, info.phnum);
        auxv!(AT_PAGESZ, 4096);
        auxv!(AT_BASE, info.at_base); // M9.7: static-PIE load base
        auxv!(AT_ENTRY, info.entry);
        auxv!(AT_RANDOM, random_ptr);
        auxv!(AT_EXECFN, execfn);
        auxv!(AT_NULL, 0);
    }

    Ok(block_start)
}

/// Map one 4 KiB page at `vaddr` (must be page-aligned) with `flags`.
fn map_page(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    vaddr: u64,
    flags: PageTableFlags,
) {
    debug_assert_eq!(vaddr & 0xFFF, 0, "user page not aligned");
    let page = Page::containing_address(VirtAddr::new(vaddr));
    let frame = frame_allocator
        .allocate_frame()
        .expect("out of physical frames while mapping userspace");
    unsafe {
        mapper
            .map_to(page, frame, flags, frame_allocator)
            .expect("map_to failed for userspace page")
            .flush();
    }
}

/// Hand-rolled ELF64 loader: validates the header, maps every PT_LOAD
/// segment (file data + zeroed BSS tail), returns `e_entry`. Returns `Err`
/// (no panic) on any malformation — disk-loaded images are user input.
fn load_elf(
    elf: &[u8],
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<ElfLoadInfo, &'static str> {
    if elf.len() < 64 {
        return Err("truncated (no ELF header)");
    }
    if !elf.starts_with(b"\x7FELF") {
        return Err("bad magic");
    }
    if elf[4] != 2 {
        return Err("not 64-bit");
    }
    if elf[5] != 1 {
        return Err("not little-endian");
    }

    let rd32 = |off: usize| -> Result<u32, &'static str> {
        elf.get(off..off + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or("phdr/header out of range")
    };
    let rd64 = |off: usize| -> Result<u64, &'static str> {
        elf.get(off..off + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or("phdr/header out of range")
    };

    // ELF64 header field offsets (64-byte header).
    let stated_entry = rd64(24)?;
    let phoff = rd64(32)? as usize;
    let phentsize = u16::from_le_bytes(
        elf.get(54..56)
            .and_then(|b| b.try_into().ok())
            .ok_or("phdr/header out of range")?,
    ) as usize;
    let phnum = u16::from_le_bytes(
        elf.get(56..58)
            .and_then(|b| b.try_into().ok())
            .ok_or("phdr/header out of range")?,
    ) as usize;
    let e_type = u16::from_le_bytes(
        elf.get(16..18)
            .and_then(|b| b.try_into().ok())
            .ok_or("phdr/header out of range")?,
    );
    if e_type != 2 && e_type != ET_DYN {
        return Err("not ET_EXEC/ET_DYN");
    }
    if phentsize < 56 {
        return Err("bad phentsize");
    }

    // M9.7: ET_DYN (static-PIE) vaddrs are *preferred*, not absolute. Compute
    // the page-rounded preferred base (min PT_LOAD vaddr), then map the image
    // at the fixed PIE load region and factor the load base through every
    // absolute address (segments, entry, AT_PHDR) so the file's offsets stay
    // as shipped. ET_EXEC keeps load_base = 0 (vaddrs are absolute).
    let mut pref = u64::MAX;
    if e_type == ET_DYN {
        for i in 0..phnum {
            let ph = match phoff.checked_add(i.checked_mul(phentsize).ok_or("phdr overflow")?) {
                Some(v) => v,
                None => return Err("phdr offset overflow"),
            };
            if ph + 56 > elf.len() {
                return Err("phdr out of range");
            }
            if rd32(ph)? != PT_LOAD {
                continue;
            }
            pref = pref.min(rd64(ph + 16)? & !0xFFF);
        }
        if pref == u64::MAX {
            return Err("no PT_LOAD segments");
        }
    } else {
        pref = 0;
    }
    let load_base = if e_type == ET_DYN { LNX_PIE_LOAD_BASE - pref } else { 0 };

    let entry = load_base + stated_entry;
    if !(USER_TEXT_BASE..USER_MAX_ADDR).contains(&entry) {
        return Err("entry point out of range");
    }

    let mut loaded = 0;
    let mut phdr_vaddr: Option<u64> = None;
    // M9.7: mapped [start, end) of every loaded PT_LOAD — the relocation
    // writer validates each target against these extents.
    let mut extents: Vec<(u64, u64)> = Vec::new();
    for i in 0..phnum {
        let ph = match phoff.checked_add(i.checked_mul(phentsize).ok_or("phdr overflow")?) {
            Some(v) => v,
            None => return Err("phdr offset overflow"),
        };
        if ph + 56 > elf.len() {
            return Err("phdr out of range");
        }
        if rd32(ph)? != PT_LOAD {
            continue;
        }
        // ELF64 program header field offsets (56-byte entry).
        let p_flags = rd32(ph + 4)?;
        let p_offset = rd64(ph + 8)? as usize;
        let p_vaddr = rd64(ph + 16)?;
        let p_filesz = rd64(ph + 32)? as usize;
        let p_memsz = rd64(ph + 40)? as usize;
        // C1: record the phdr table's vaddr when it lies inside this
        // segment's file bytes (static binaries: first PT_LOAD, always). The
        // vaddr is a file-space offset (preferred for ET_DYN), so the mapped
        // address is load_base + offset.
        if phdr_vaddr.is_none() && phoff >= p_offset && (phoff - p_offset) < p_filesz {
            phdr_vaddr = Some(load_base + p_vaddr + (phoff - p_offset) as u64);
        }
        if p_offset.checked_add(p_filesz).ok_or("size overflow")? > elf.len() {
            return Err("segment data out of range");
        }
        if p_memsz < p_filesz {
            return Err("memsz < filesz");
        }

        // M9.7: the mapped segment sits at load_base + the file's vaddr
        // (load_base = 0 for ET_EXEC). checked_add defends against absurd file
        // vaddrs on a shared address space.
        let map_va = match load_base.checked_add(p_vaddr) {
            Some(v) => v,
            None => return Err("segment vaddr overflow"),
        };
        let start = map_va & !0xFFF;
        let end = match p_memsz.checked_add(0xFFF).and_then(|m| map_va.checked_add(m as u64)) {
            Some(v) => v & !0xFFF,
            None => return Err("segment size overflow"),
        };
        if end <= start || start < USER_TEXT_BASE || end > USER_MAX_ADDR {
            return Err("segment outside reserved region");
        }

        // Map writable first: the segment contents must be copied in, and
        // CR0.WP=1 makes even supervisor writes respect the W bit. Tighten
        // to the segment's final protection after the copy.
        let mut map_flags = PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITABLE;
        if p_flags & PF_X == 0 {
            map_flags |= PageTableFlags::NO_EXECUTE;
        }
        let mut final_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if p_flags & PF_W != 0 {
            final_flags |= PageTableFlags::WRITABLE;
        }
        if p_flags & PF_X == 0 {
            final_flags |= PageTableFlags::NO_EXECUTE;
        }
        let mut va = start;
        while va < end {
            map_page(mapper, frame_allocator, va, map_flags);
            va += 0x1000;
        }

        unsafe {
            let dst = map_va as *mut u8;
            core::ptr::copy_nonoverlapping(elf.as_ptr().add(p_offset), dst, p_filesz);
            core::ptr::write_bytes(dst.add(p_filesz), 0, p_memsz - p_filesz);
        }
        extents.push((start, end));

        if final_flags != map_flags {
            let mut va = start;
            while va < end {
                let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(va));
                unsafe {
                    mapper
                        .update_flags(page, final_flags)
                        .expect("failed to apply final segment flags")
                        .flush();
                }
                va += 0x1000;
            }
        }
        serial_writeln!(
            "userspace: PT_LOAD vaddr={:#x} filesz={:#x} memsz={:#x} flags={:#x}",
            p_vaddr,
            p_filesz,
            p_memsz,
            p_flags
        );
        loaded += 1;
    }
    if loaded == 0 {
        return Err("no PT_LOAD segments");
    }
    // M9.7: ET_DYN needs its data relocations applied before it can run —
    // R_X86_64_RELATIVE pointer slots carry no usable value, and GLOB_DAT /
    // JUMP_SLOT GOT entries are zero until resolved through .dynsym. C1:
    // AT_PHDR — the program-header table's virtual address when it lies
    // inside a mapped segment (static binaries: always the first PT_LOAD).
    if e_type == ET_DYN {
        apply_relative_relocations(elf, load_base, &extents)?;
    }
    Ok(ElfLoadInfo {
        entry,
        phnum: phnum as u64,
        phdr_vaddr,
        at_base: if e_type == ET_DYN { LNX_PIE_LOAD_BASE } else { 0 },
    })
}

/// M9.7: apply an ET_DYN image's data relocations before entry:
///  - R_X86_64_RELATIVE (8):   *slot = load_base + addend
///  - R_X86_64_GLOB_DAT (6) /
///    R_X86_64_JUMP_SLOT (7):  *slot = load_base + st_value + addend,
///                               resolved through the image's own .dynsym
///                               (static-PIE defines every symbol locally;
///                               a GLOB_DAT for `main` is exactly what rust-lld
///                               emits for the _start -> main call of a
///                               -shared -static link).
/// Every target must land inside a loaded segment's mapped extent — a write
/// elsewhere (e.g. a fresh page) would silently corrupt the shared user
/// address space, and an unmapped target must never fault the kernel.
fn apply_relative_relocations(
    elf: &[u8],
    load_base: u64,
    extents: &[(u64, u64)],
) -> Result<(), &'static str> {
    const SHT_DYNSYM: u32 = 11;
    const R_X86_64_GLOB_DAT: u64 = 6;
    const R_X86_64_JUMP_SLOT: u64 = 7;
    let rd32 = |off: usize| -> Result<u32, &'static str> {
        elf.get(off..off + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or("rela section out of range")
    };
    let rd64 = |off: usize| -> Result<u64, &'static str> {
        elf.get(off..off + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or("rela section out of range")
    };
    let shoff = rd64(40)? as usize;
    let shentsize = u16::from_le_bytes(
        elf.get(58..60)
            .and_then(|b| b.try_into().ok())
            .ok_or("section header out of range")?,
    ) as usize;
    let shnum = u16::from_le_bytes(
        elf.get(60..62)
            .and_then(|b| b.try_into().ok())
            .ok_or("section header out of range")?,
    ) as usize;
    if shoff == 0 || shentsize < 64 {
        return Ok(()); // no section table: nothing to apply
    }

    // Locate .dynsym (SHT_DYNSYM) for symbol-based relocation classes. Its
    // sh_link names .dynstr, which we keep so failures can name the symbol —
    // an ET_DYN with an unresolved import must fail loudly WITH a name, not
    // send the task to a RIP=0 instruction fetch.
    let mut dynsym: Option<(usize, usize)> = None; // (offset, size)
    let mut dynstr: Option<(usize, usize)> = None; // (offset, size)
    for i in 0..shnum {
        let sh = match shoff.checked_add(i.checked_mul(shentsize).ok_or("shdr overflow")?) {
            Some(v) => v,
            None => return Err("shdr offset overflow"),
        };
        if sh + 64 > elf.len() {
            return Err("shdr out of range");
        }
        if rd32(sh + 4)? == SHT_DYNSYM {
            dynsym = Some((rd64(sh + 24)? as usize, rd64(sh + 32)? as usize));
            let link = rd32(sh + 40)? as usize; // -> .dynstr section index
            let strh = match shoff.checked_add(link.checked_mul(shentsize).ok_or("shdr overflow")?)
            {
                Some(v) => v,
                None => return Err("shdr offset overflow"),
            };
            if strh + 64 <= elf.len() {
                dynstr = Some((rd64(strh + 24)? as usize, rd64(strh + 32)? as usize));
            }
            break;
        }
    }

    // Elf64_Sym: st_name@0, st_shndx@6 (u16), st_value@8 (u64).
    let sym_name = |sym_idx: u64| -> Result<&'static str, &'static str> {
        let (off, size) = dynsym.ok_or("symbol reloc without .dynsym")?;
        let idx = sym_idx
            .checked_mul(24)
            .ok_or("sym index overflow")? as usize;
        let s = off.checked_add(idx).ok_or("sym out of range")?;
        if s + 24 > off + size || s + 24 > elf.len() {
            return Err("sym out of range");
        }
        let st_name = rd32(s)? as usize;
        let (str_off, str_size) = dynstr.ok_or("symbol reloc without .dynstr")?;
        let base = str_off.checked_add(st_name).ok_or("sym name out of range")?;
        if base >= str_off + str_size || base >= elf.len() {
            return Err("sym name out of range");
        }
        let end = elf[base..(str_off + str_size).min(elf.len())]
            .iter()
            .position(|&b| b == 0)
            .map(|p| base + p)
            .ok_or("unterminated sym name")?;
        // Leaked (the heap exists by user-program load time; a handful of
        // failure diagnostics is acceptable).
        Ok(alloc::string::String::from_utf8_lossy(
            &elf[base..end.min(base + 32)],
        )
        .into_owned()
        .leak())
    };

    // Elf64_Sym: st_name@0, st_shndx@6 (u16), st_value@8 (u64).
    let sym_addr = |sym_idx: u64| -> Result<u64, &'static str> {
        let (off, size) = dynsym.ok_or("symbol reloc without .dynsym")?;
        let idx = sym_idx
            .checked_mul(24)
            .ok_or("sym index overflow")? as usize;
        let s = off.checked_add(idx).ok_or("sym out of range")?;
        if s + 24 > off + size || s + 24 > elf.len() {
            return Err("sym out of range");
        }
        let shndx = u16::from_le_bytes(
            elf.get(s + 6..s + 8)
                .and_then(|b| b.try_into().ok())
                .ok_or("sym out of range")?,
        );
        if shndx == 0 {
            // Name the missing import so the boot log explains the refusal
            // (an ET_DYN with an unresolved import must never reach ring 3).
            let name = sym_name(sym_idx).unwrap_or("?");
            serial_writeln!("userspace: ET_DYN reloc for UNDEFINED symbol '{name}'");
            return Err("symbol reloc for undefined symbol");
        }
        rd64(s + 8)
    };

    let mut applied = 0u32;
    for i in 0..shnum {
        let sh = match shoff.checked_add(i.checked_mul(shentsize).ok_or("shdr overflow")?) {
            Some(v) => v,
            None => return Err("shdr offset overflow"),
        };
        if sh + 64 > elf.len() {
            return Err("shdr out of range");
        }
        if rd32(sh + 4)? != SHT_RELA {
            continue;
        }
        let sh_offset = rd64(sh + 24)? as usize;
        let sh_size = rd64(sh + 32)? as usize;
        let mut off = sh_offset;
        let rela_end = match sh_offset.checked_add(sh_size).and_then(|e| elf.get(0..e)) {
            Some(_) => sh_offset + sh_size,
            None => return Err("rela section out of range"),
        };
        while off + 24 <= rela_end {
            let r_offset = rd64(off)?;
            let r_info = rd64(off + 8)?;
            let r_type = r_info & 0xFFFF_FFFF;
            let r_addend = i64::from_le_bytes(
                elf.get(off + 16..off + 24)
                    .and_then(|b| b.try_into().ok())
                    .ok_or("rela entry out of range")?,
            );
            let value = match r_type {
                R_X86_64_RELATIVE => (load_base as i64).wrapping_add(r_addend),
                R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT => (load_base as i64)
                    .wrapping_add(sym_addr(r_info >> 32)? as i64)
                    .wrapping_add(r_addend),
                _ => {
                    off += 24;
                    continue;
                }
            };
            let target = match load_base.checked_add(r_offset) {
                Some(v) => v,
                None => return Err("relocation target overflow"),
            };
            if !extents.iter().any(|(s, e)| target >= *s && target + 8 <= *e) {
                return Err("relocation target outside loaded segments");
            }
            unsafe {
                (target as *mut u64).write_volatile(value as u64);
            }
            applied += 1;
            off += 24;
        }
    }
    serial_writeln!(
        "userspace: ET_DYN load_base={load_base:#x}: applied {applied} relocations (RELATIVE/GLOB_DAT/JUMP_SLOT)"
    );
    Ok(())
}

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

use crate::{memory, scheduler, serial_writeln, vfs};

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

/// Page-aligned virtual extent of `elf`'s PT_LOAD segments. Defensive (no
/// panics): `None` on any malformation — a disk-loaded ELF must never take
/// the kernel down in this pre-validation step.
fn elf_region(elf: &[u8]) -> Option<(u64, u64)> {
    if elf.len() < 64 || !elf.starts_with(b"\x7FELF") || elf[4] != 2 || elf[5] != 1 {
        return None;
    }
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
    if lo == u64::MAX || hi <= lo || lo < USER_TEXT_BASE || hi > USER_MAX_ADDR {
        return None;
    }
    Some((lo, hi))
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
    Ok(scheduler::spawn_user(info.entry, rsp))
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
}

// auxv keys (Linux ABI subset).
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
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
    let block = 8 * (3 + argv.len() + envp.len()) + 8 * 16; // argc+argv+envp slots + auxv
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
    let pairs: [(u64, u64); 8] = [
        (AT_PHDR, info.phdr_vaddr.unwrap_or(0)),
        (AT_PHENT, 56),
        (AT_PHNUM, info.phnum),
        (AT_PAGESZ, 4096),
        (AT_ENTRY, info.entry),
        (AT_RANDOM, random_ptr),
        (AT_EXECFN, execfn),
        (AT_NULL, 0),
    ];

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
        for (t, v) in pairs {
            *w = t;
            w = w.add(1);
            *w = v;
            w = w.add(1);
        }
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
    let entry = rd64(24)?;
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
    if !(USER_TEXT_BASE..USER_MAX_ADDR).contains(&entry) {
        return Err("entry point out of range");
    }
    if phentsize < 56 {
        return Err("bad phentsize");
    }

    let mut loaded = 0;
    let mut phdr_vaddr: Option<u64> = None;
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
        // segment's file bytes (static binaries: first PT_LOAD, always).
        if phdr_vaddr.is_none() && phoff >= p_offset && (phoff - p_offset) < p_filesz {
            phdr_vaddr = Some(p_vaddr + (phoff - p_offset) as u64);
        }
        if p_offset.checked_add(p_filesz).ok_or("size overflow")? > elf.len() {
            return Err("segment data out of range");
        }
        if p_memsz < p_filesz {
            return Err("memsz < filesz");
        }

        let start = p_vaddr & !0xFFF;
        let end = (p_vaddr + p_memsz as u64 + 0xFFF) & !0xFFF;
        if start < USER_TEXT_BASE || end > USER_MAX_ADDR {
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
            let dst = p_vaddr as *mut u8;
            core::ptr::copy_nonoverlapping(elf.as_ptr().add(p_offset), dst, p_filesz);
            core::ptr::write_bytes(dst.add(p_filesz), 0, p_memsz - p_filesz);
        }

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
    // C1: AT_PHDR — the program-header table's virtual address when it lies
    // inside a mapped segment (static binaries: always the first PT_LOAD).
    Ok(ElfLoadInfo {
        entry,
        phnum: phnum as u64,
        phdr_vaddr,
    })
}

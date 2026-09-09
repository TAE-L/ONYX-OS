//! Physical memory management: a frame allocator built from the bootloader's
//! memory map, plus paging infrastructure so the kernel can map virtual pages
//! to physical frames on demand.
//!
//! The bootloader is configured (see `main.rs`) to map the complete physical
//! memory somewhere in the virtual address space; `boot_info.physical_memory_offset`
//! tells us where, which lets us access page tables through virtual addresses.

use bootloader_api::info::{BootInfo, MemoryRegion, MemoryRegionKind, Optional};
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::paging::{
    FrameAllocator, OffsetPageTable, PageSize, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::PhysAddr;
use x86_64::VirtAddr;

/// A frame allocator that hands out usable physical frames from the memory map
/// provided by the bootloader. Only frames in `Usable` regions are returned,
/// because the bootloader/kernel/firmware live in the other regions.
///
/// A6 v2 — a two-tier O(1) design replacing the M2 bump allocator (which
/// re-walked every usable region on *every* allocation — O(frames-so-far) —
/// and could never free):
///
/// 1. **Intrusive free list.** Deallocated frames are pushed onto a
///    singly-linked stack whose next-pointers live in the free frames
///    themselves (first 8 bytes, accessed through the physical-memory
///    window) — no heap needed, O(1) push/pop, and `allocate_frame` serves
///    from it first.
/// 2. **Region bump.** Fresh allocations advance a monotonic cursor across
///    the usable regions: O(usable regions) per allocation (a handful)
///    instead of v1's O(frames).
///
/// Single-owner `&mut` discipline: the allocator lives in `GLOBAL_FRAMES`
/// and is reached only through [`with_global_frames`], which runs its
/// closure with interrupts disabled so a preemption can never interleave a
/// second `&mut` view. Never call from IRQ context.
pub struct BootInfoFrameAllocator {
    memory_regions: &'static [MemoryRegion],
    /// Physical address of the next never-touched frame (bump path).
    bump_next: u64,
    /// Physical address of the first free frame on the intrusive list,
    /// `0` = empty. A free frame stores the previous head in its first
    /// 8 bytes (written through the physical-memory window).
    free_head: u64,
    /// Usable frame count, computed once at init from the regions.
    total: u64,
    /// Net outstanding allocations (allocated - deallocated).
    used: u64,
    /// Cumulative deallocations (observability).
    freed: u64,
    /// Cumulative allocations that returned `None` (OOM observability).
    oom: u64,
}

impl BootInfoFrameAllocator {
    /// Creates a frame allocator from the memory regions in `BootInfo`.
    ///
    /// # Safety
    /// `memory_regions` must point to valid memory for the 'static lifetime
    /// (it comes from the bootloader). Only `Usable` regions are returned.
    /// Same contract as v1: created before any allocation happens, and no
    /// other code may touch `Usable` frames.
    pub unsafe fn init(memory_regions: &'static [MemoryRegion]) -> Self {
        let total: u64 = memory_regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .map(|r| ((r.end as u64) - (r.start as u64)) / Size4KiB::SIZE)
            .sum();
        Self {
            memory_regions,
            bump_next: 0,
            free_head: 0,
            total,
            used: 0,
            freed: 0,
            oom: 0,
        }
    }

    /// Read a u64 from physical memory through the bootloader's phys window.
    fn phys_read(addr: u64) -> u64 {
        let off = PHYS_OFFSET.load(Ordering::Relaxed);
        assert!(off != 0, "frame allocator used before memory::init");
        unsafe { ((off + addr) as *const u64).read_volatile() }
    }

    /// Write a u64 to physical memory through the bootloader's phys window.
    fn phys_write(addr: u64, val: u64) {
        let off = PHYS_OFFSET.load(Ordering::Relaxed);
        assert!(off != 0, "frame allocator used before memory::init");
        unsafe { ((off + addr) as *mut u64).write_volatile(val) }
    }

    /// Snapshot: `(used, total, freed, oom)`. `used` is net outstanding;
    /// free frames = total - used (bump remainder + free list combined).
    pub fn stats(&self) -> (u64, u64, u64, u64) {
        (self.used, self.total, self.freed, self.oom)
    }

    /// Return a frame to the free list (A6 v2 — the x86_64 `FrameAllocator`
    /// trait has no deallocation, so this is an inherent extension). O(1):
    /// the frame becomes the new free-list head, its first 8 bytes now hold
    /// the previous head (written through the physical-memory window).
    ///
    /// # Safety
    /// `frame` must have been handed out by THIS allocator and must not be
    /// reachable by any page-table mapping or CPU access anymore — a
    /// double-free or a free of an in-use frame corrupts the free list.
    /// Call only from task/syscall context (IF=0), never IRQ context.
    pub unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        let addr = frame.start_address().as_u64();
        Self::phys_write(addr, self.free_head);
        self.free_head = addr;
        self.used -= 1;
        self.freed += 1;
    }
}

// SAFETY: a frame is returned at most once until freed: fresh frames come
// from a monotonic bump cursor (never revisited), reused frames are popped
// from a free list they can be on only once. Double-free is a caller
// contract violation of the unsafe `deallocate_frame` and corrupts the list.
unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        // 1. Free list first: O(1).
        if self.free_head != 0 {
            let addr = self.free_head;
            self.free_head = Self::phys_read(addr);
            self.used += 1;
            return Some(PhysFrame::containing_address(PhysAddr::new(addr)));
        }
        // 2. Bump across the usable regions: O(usable regions) — scanning
        //    always starts at the lowest region, but `bump_next` never
        //    regresses, so drained regions are skipped by the max() below.
        let size = Size4KiB::SIZE;
        for r in self.memory_regions {
            if r.kind != MemoryRegionKind::Usable {
                continue;
            }
            let r_start = r.start as u64;
            let r_end = r.end as u64;
            let aligned_start = (r_start + size - 1) / size * size;
            let cand = aligned_start.max(self.bump_next);
            if cand + size <= r_end {
                self.bump_next = cand + size;
                self.used += 1;
                return Some(PhysFrame::containing_address(PhysAddr::new(cand)));
            }
        }
        self.oom += 1;
        None
    }
}

/// Initialize paging and return a mapper over the active page tables.
///
/// The bootloader must be built with `mappings.physical_memory = Some(Dynamic)`
/// so that `boot_info.physical_memory_offset` is set.
pub fn init(boot_info: &'static mut BootInfo) -> OffsetPageTable<'static> {
    let phys_offset = match boot_info.physical_memory_offset {
        Optional::Some(offset) => VirtAddr::new(offset),
        Optional::None => panic!("physical memory not mapped by bootloader"),
    };
    // Remember the offset so `runtime_mapper` can rebuild a mapper at
    // post-boot time (SYS_SPAWN context).
    PHYS_OFFSET.store(phys_offset.as_u64(), Ordering::Relaxed);

    let level_4_table = unsafe { active_level_4_table(phys_offset) };
    unsafe { OffsetPageTable::new(level_4_table, phys_offset) }
}

/// The physical-memory virtual offset recorded at boot (0 = not initialized).
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Build a mapper over the *currently active* page tables (the kernel runs a
/// single address space). Post-boot (SYS_SPAWN) context only, IF=0: the
/// returned view intentionally aliases the boot-time mapper — there is no
/// concurrent mapping anywhere else while a syscall runs.
pub fn runtime_mapper() -> Option<OffsetPageTable<'static>> {
    let off = PHYS_OFFSET.load(Ordering::Relaxed);
    if off == 0 {
        return None;
    }
    let level_4_table = unsafe { active_level_4_table(VirtAddr::new(off)) };
    Some(unsafe { OffsetPageTable::new(level_4_table, VirtAddr::new(off)) })
}

/// Post-boot frame allocator snapshot (moved here by `main` once boot-time
/// allocations are done). Single-owner: only accessed with IF=0 in syscall
/// context, so `static mut` follows the established kernel pattern.
static mut GLOBAL_FRAMES: Option<BootInfoFrameAllocator> = None;

/// Hand the boot frame allocator over to the runtime snapshot. Call once,
/// after the last boot-path use of the allocator.
pub fn init_global_frames(allocator: BootInfoFrameAllocator) {
    unsafe {
        *(&raw mut GLOBAL_FRAMES) = Some(allocator);
    }
}

/// Run `f` with the global (post-boot) frame allocator. `None` if the
/// snapshot was never installed (boot phase still owns it).
///
/// Runs the closure with interrupts disabled (A6): the allocator is handed
/// out as a bare `&mut`, so a preemption mid-closure would let the next task
/// open a second `&mut` view of the same allocator — free-list corruption.
/// The established callers (the SYS_SPAWN syscall path) already run at IF=0;
/// this makes the guarantee hold for every future caller.
pub fn with_global_frames<R>(f: impl FnOnce(&mut BootInfoFrameAllocator) -> R) -> Option<R> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let frames = unsafe { (&raw mut GLOBAL_FRAMES).as_mut() }?.as_mut()?;
        Some(f(frames))
    })
}

/// Frame-allocator stats from the runtime snapshot: `(used, total, freed,
/// oom)`. `None` before `init_global_frames` (boot phase still owns it).
pub fn frame_stats() -> Option<(u64, u64, u64, u64)> {
    with_global_frames(|f| f.stats())
}


/// Returns a mutable reference to the active level 4 page table, using the
/// physical-memory offset to translate its (physical) address into the virtual
/// address space.
///
/// # Safety
/// `physical_memory_offset` must be the offset configured in the bootloader so
/// that `offset + phys` correctly maps physical memory.
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_frame, _flags) = Cr3::read();
    let phys = level_4_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *ptr
}

/// Where the bootloader mapped the complete physical memory (`Some(off)` =
/// every physical address `p` is visible at virtual `off + p`), `None` before
/// `memory::init`. Device MMIO (LAPIC/IOAPIC) is accessed THROUGH this
/// existing window — creating fresh page-table entries for MMIO would risk
/// replacing an entry of a shared table in the bootloader's dynamic mapping.
pub fn phys_offset() -> Option<u64> {
    let off = PHYS_OFFSET.load(Ordering::Relaxed);
    if off == 0 {
        None
    } else {
        Some(off)
    }
}

/// Page-table at `phys`, seen through the physical-memory window.
/// # Safety: caller guarantees `off` is the live physical-memory offset.
unsafe fn table_at(off: u64, phys: PhysAddr) -> Option<&'static mut PageTable> {
    let virt = off.checked_add(phys.as_u64())?;
    Some(&mut *(VirtAddr::new(virt).as_mut_ptr::<PageTable>()))
}

/// Page-table flags of the 4 KiB page containing `vaddr` in the *active*
/// address space, or `None` if that page is not mapped to a present 4 KiB
/// page (huge-page mappings also read as `None`: userspace never creates
/// them). Used by the syscall layer to validate user pointers before the
/// kernel dereferences them. Syscall context (IF=0) only — the walk assumes
/// the address space cannot change underneath it.
pub fn page_flags(vaddr: VirtAddr) -> Option<PageTableFlags> {
    let off = PHYS_OFFSET.load(Ordering::Relaxed);
    if off == 0 {
        return None;
    }
    unsafe {
        let p4 = active_level_4_table(VirtAddr::new(off));
        let p4e = &p4[vaddr.p4_index()];
        if !p4e.flags().contains(PageTableFlags::PRESENT) {
            return None;
        }
        let p3 = table_at(off, p4e.frame().ok()?.start_address())?;
        let p3e = &p3[vaddr.p3_index()];
        if !p3e.flags().contains(PageTableFlags::PRESENT)
            || p3e.flags().contains(PageTableFlags::HUGE_PAGE)
        {
            return None;
        }
        let p2 = table_at(off, p3e.frame().ok()?.start_address())?;
        let p2e = &p2[vaddr.p2_index()];
        if !p2e.flags().contains(PageTableFlags::PRESENT)
            || p2e.flags().contains(PageTableFlags::HUGE_PAGE)
        {
            return None;
        }
        let p1 = table_at(off, p2e.frame().ok()?.start_address())?;
        let p1e = &p1[vaddr.p1_index()];
        if !p1e.flags().contains(PageTableFlags::PRESENT) {
            return None;
        }
        Some(p1e.flags())
    }
}
//! Kernel heap: a global allocator backed by a fixed region of mapped pages.
//!
//! We place the heap at a high, page-aligned virtual address that the
//! bootloader leaves unused (`0x4444_4444_0000`), map each heap page to a
//! freshly allocated physical frame, and hand the region to a linked-list
//! allocator. This makes `Box`, `Vec`, `String`, ... available in the kernel.

use linked_list_allocator::LockedHeap;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB,
};
use x86_64::VirtAddr;

/// Virtual address where the heap starts (page-aligned, unused by bootloader).
pub const HEAP_START: usize = 0x_4444_4444_0000;
/// Heap size: 16 MiB (A6 v2 — grown from 2 MiB; the O(1) frame allocator
/// makes the 4096-page boot mapping cheap, and the block cache / ELF loaders
/// / future subsystems want the headroom).
pub const HEAP_SIZE: usize = 16 * 1024 * 1024;

/// The global heap allocator used by `alloc` (Vec/String/Box/...).
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Map `HEAP_SIZE` pages at `HEAP_START` and initialize the global allocator.
///
/// Must be called after paging is set up (see `memory::init`) and before any
/// heap allocation.
pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) {
    let heap_start = VirtAddr::new(HEAP_START as u64);
    let heap_end = heap_start + HEAP_SIZE as u64;

    // Map every page of the heap region to a freshly allocated frame.
    let heap_pages = Page::range_inclusive(
        Page::containing_address(heap_start),
        Page::containing_address(heap_end - 1u64),
    );
    for page in heap_pages {
        let frame = frame_allocator
            .allocate_frame()
            .expect("no frames left for heap pages");
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .expect("failed to map heap page")
                .flush();
        }
    }

    // Now the region is mapped: give it to the allocator.
    unsafe {
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }
}

/// B5: current heap usage (used, free) in bytes. Snapshot for the perf
/// report; locks the allocator briefly — task/syscall context only.
pub fn heap_stats() -> (usize, usize) {
    let heap = ALLOCATOR.lock();
    (heap.used(), heap.free())
}
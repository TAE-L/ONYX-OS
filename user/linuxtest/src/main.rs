// M9.7 regression: a REAL Linux-ABI binary. Compiled by the root build.rs with
//   rustc --target x86_64-unknown-linux-gnu + rust-lld -shared -static
// producing an ET_DYN (static-PIE) image: preferred vaddrs of 0, which the
// kernel's loader relocates into the PIE region, and R_X86_64_RELATIVE
// relocations the kernel must apply before entry. It speaks the raw Linux
// syscall table (rax = number, arg4 in r10, -errno returns) and the kernel
// serves it through the M9.7 shim - zero Onyx-native syscalls used.
//
// Also carries the literal marker ONYXLNX\0 (the loader's Linux-ABI sniff)
// and prints [lnxtest] markers the boot-test greps for.
#![no_std]
#![no_main]

use core::arch::asm;

// The loader sniffs this marker to route the task through the Linux shim.
#[used]
static MARK: &[u8] = b"ONYXLNX\0";

// M9.7 relocation probe: the fat pointer lives in .rodata as a pointer slot
// patched by R_X86_64_RELATIVE (addend = address of the bytes). Read through
// the slot at runtime; if the kernel skipped the relocations, the pointer is
// the unrelocated low address and the read faults/mismatches.
#[used]
static PIE_MSG: &[u8] = b"reloc: LNX-PIE pointer OK (kernel applied R_X86_64_RELATIVE)\n";

core::arch::global_asm!(
    ".globl _start",
    ".type _start, @function",
    "_start:",
    // System V process start: rsp -> { argc, argv[0..], NULL, envp..., NULL, auxv }
    "mov rdi, rsp",
    "call {main_}",
    "9: jmp 9b", // main never returns; belt-and-braces trap
    main_ = sym linuxtest_main,
);

/// Entry from _start: `sp` points at the process-start stack.
// #[no_mangle] matters: the global-asm reference and this definition must
// resolve to the same symbol, or the linker emits a GOT entry importing an
// undefined "main" (shared objects allow unresolved imports) ??? which the
// kernel's loader rightly rejects with "symbol reloc for undefined symbol".
#[unsafe(no_mangle)]
extern "sysv64" fn linuxtest_main(sp: *const u64) {
    let (argc, argv) = unsafe {
        let argc = *sp as usize;
        let mut argv = [0u64; 8];
        for i in 0..argc.min(8) {
            argv[i] = *sp.add(1 + i);
        }
        (argc, argv)
    };
    if run(argc, &argv) {
        puts(b"[lnxtest] PASSED\n");
        exit(0);
    }
    exit(1);
}

fn run(argc: usize, argv: &[u64]) -> bool {
    if argc != 1 {
        puthexln(b"argc", argc as u64);
        return false;
    }
    // argv[0] deref works only if the kernel built the process-start stack.
    if argv[0] == 0 {
        puts(b"[lnxtest] FAILED: argv[0] == 0\n");
        return false;
    }

    // --- brk (12) ---
    let b0 = syscall1(12, 0);
    let want = b0.wrapping_add(0x2000);
    let b1 = syscall1(12, want);
    if b1 < want {
        puts(b"[lnxtest] FAILED: brk\n");
        return false;
    }
    puts(b"brk: OK\n");

    // --- anonymous mmap (9) ---
    // mmap(NULL, 0x1000, PROT_RW, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    let page = syscall6(9, 0, 0x1000, 3, 0x22, u64::MAX, 0);
    if page == u64::MAX || page & 0xFFF != 0 {
        puts(b"[lnxtest] FAILED: mmap\n");
        return false;
    }
    // write + read back through the mapping (demand-paged by the kernel).
    unsafe { (page as *mut u64).write_volatile(0xDEAD_BEEF_CAFE_F00D) };
    if unsafe { (page as *mut u64).read_volatile() } != 0xDEAD_BEEF_CAFE_F00D {
        puts(b"[lnxtest] FAILED: mmap write-back\n");
        return false;
    }
    puts(b"mmap: OK\n");

    // --- arch_prctl TLS (158 / ARCH_SET_FS) ---
    // Set FS to the mmap'd page, then prove it is live with a fs-relative
    // load (TLS accesses go through fs:[..]).
    if syscall2(158, 0x1002, page) != 0 {
        puts(b"[lnxtest] FAILED: ARCH_SET_FS\n");
        return false;
    }
    unsafe { (page as *mut u64).write_volatile(0x1234_5678_9ABC_DEF0) };
    let fs: u64;
    unsafe { asm!("mov {}, fs:0", out(reg) fs, options(nostack)) };
    if fs != 0x1234_5678_9ABC_DEF0 {
        puts(b"[lnxtest] FAILED: fs-relative load\n");
        return false;
    }
    puts(b"tls: OK\n");

    // --- PIE relocation probe (through the patched pointer slot) ---
    let fat: &[u8] = unsafe { core::ptr::read_volatile(&PIE_MSG) };
    puts(fat); // writes the message via the relocated pointer
    puts(b"reloc: OK\n");

    // --- getpid (39) ---
    if syscall0(39) == 0 {
        puts(b"[lnxtest] FAILED: getpid\n");
        return false;
    }
    puts(b"getpid: OK\n");

    // --- getrandom (318) ---
    let mut rnd = [0u8; 16];
    if syscall3(318, rnd.as_mut_ptr() as u64, 16, 0) != 16 {
        puts(b"[lnxtest] FAILED: getrandom\n");
        return false;
    }
    puts(b"getrandom: OK\n");

    // --- clock_gettime (228, CLOCK_MONOTONIC) ---
    let mut ts = [0u64; 2];
    if syscall2(228, 1, ts.as_mut_ptr() as u64) != 0 {
        puts(b"[lnxtest] FAILED: clock_gettime\n");
        return false;
    }
    puts(b"clock: OK\n");

    puts(b"[lnxtest] all probes passed\n");
    true
}

// --- C ABI mem functions -----------------------------------------------------
// rustc lowers bulk copies for the linux-gnu target to calls to libc's
// mem* functions, and a `-shared` link keeps those as imports (the GLOB_DAT
// for `memcpy` above). A static-PIE has no dynamic linker, so the image must
// define them itself. Volatile byte loops: LLVM must not "optimize" them back
// into calls to themselves.

#[unsafe(no_mangle)]
pub extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        unsafe { dst.add(i).write_volatile(src.add(i).read_volatile()) };
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    memcpy(dst, src, n)
}

#[unsafe(no_mangle)]
pub extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        unsafe { dst.add(i).write_volatile(c as u8) };
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (x, y) = unsafe { (a.add(i).read_volatile(), b.add(i).read_volatile()) };
        if x != y {
            return x as i32 - y as i32;
        }
        i += 1;
    }
    0
}

// --- tiny ring-3 stdlib: raw Linux syscalls + serial printing ---------------

#[inline]
fn syscall0(n: u64) -> u64 {
    let r;
    unsafe { asm!("syscall", inlateout("rax") n => r, lateout("rcx") _, lateout("r11") _, options(nostack)) };
    r
}

#[inline]
fn syscall1(n: u64, a: u64) -> u64 {
    let r;
    unsafe {
        asm!("syscall", inlateout("rax") n => r, in("rdi") a, lateout("rcx") _, lateout("r11") _, options(nostack))
    };
    r
}

#[inline]
fn syscall2(n: u64, a: u64, b: u64) -> u64 {
    let r;
    unsafe {
        asm!("syscall", inlateout("rax") n => r, in("rdi") a, in("rsi") b, lateout("rcx") _, lateout("r11") _, options(nostack))
    };
    r
}

#[inline]
fn syscall3(n: u64, a: u64, b: u64, c: u64) -> u64 {
    let r;
    unsafe {
        asm!("syscall", inlateout("rax") n => r, in("rdi") a, in("rsi") b, in("rdx") c, lateout("rcx") _, lateout("r11") _, options(nostack))
    };
    r
}

#[inline]
fn syscall6(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64 {
    let r;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => r,
            in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        )
    };
    r
}

/// write(1, buf, len) - Linux fd 1 is the serial console.
fn puts(s: &[u8]) {
    syscall3(1, 1, s.as_ptr() as u64, s.len() as u64);
}

/// hex formatting without std: prints `tag=0x<v>\n`.
fn puthexln(tag: &[u8], v: u64) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [0u8; 36];
    // Manual byte loop: slice copy_from_slice lowers to a `memcpy` call,
    // which -shared leaves an undefined *import* (compiler_builtins is not
    // implicitly pulled into a shared object) ??? the loader rejects unresolved
    // GOT imports, and rightly so.
    let mut i = 0;
    while i < tag.len() {
        buf[i] = tag[i];
        i += 1;
    }
    buf[tag.len()] = b'=';
    buf[tag.len() + 1] = b'0';
    buf[tag.len() + 2] = b'x';
    for (i, nib) in (0..16).rev().enumerate() {
        buf[tag.len() + 3 + i] = HEX[((v >> (nib * 4)) & 0xF) as usize];
    }
    buf[tag.len() + 19] = b'\n';
    puts(&buf[..tag.len() + 20]);
}

fn exit(code: u64) -> ! {
    syscall1(231, code); // exit_group
    loop {
        unsafe { asm!("hlt", options(nostack, nomem)) }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts(b"[lnxtest] PANIC\n");
    exit(2);
}




//! M8: the OnyxOS ring-3 shell, loaded from the FAT32 partition by the
//! kernel's disk-ELF loader (not embedded in the kernel).
//!
//! Startup: executes `/AUTOEXEC.TXT` line-by-line if present (how the
//! headless tests drive it), then an interactive prompt via SYS_READ(fd 0).
//! Commands: ls, mkdir, cat, write, run, echo, help, exit.
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const SYS_WRITE: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_OPEN: u64 = 2;
const SYS_READ: u64 = 3;
const SYS_CLOSE: u64 = 4;
const SYS_LS: u64 = 5;
const SYS_MKDIR: u64 = 6;
const SYS_LSPCI: u64 = 12;
const SYS_SPAWN: u64 = 7;
// BUG FIX (B3 session): SYS_FLUSH was defined as 12 here — the shell's `sync`
// was silently invoking SYS_LSPCI instead (both print output and return 0, so
// it looked like it worked). Corrected to the kernel's 13.
const SYS_FLUSH: u64 = 13;
const SYS_WAITPID: u64 = 14;
const SYS_KILL: u64 = 15;
const SYS_PERF: u64 = 17;
// C3: file API completion.
const SYS_STAT: u64 = 18;
const SYS_UNLINK: u64 = 20;
const SYS_RENAME: u64 = 21;

const AUTOEXEC: &[u8] = b"/AUTOEXEC.TXT";
const FD_CONSOLE: u64 = 1;
// C2: syscalls return -errno on error; anything negative is an error
// (the old u64::MAX sentinel is just the generic -1 = -EPERM).
fn is_err(v: u64) -> bool {
    (v as i64) < 0
}

/// Errno value for -ENOENT (2), used to report missing files.
const ENOENT: u64 = 2;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[inline(never)]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

#[inline(never)]
unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r8") a4, // `syscall` clobbers rcx/r11, but r8 survives
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

fn say(msg: &[u8]) {
    unsafe {
        let _ = syscall3(SYS_WRITE, FD_CONSOLE, msg.as_ptr() as u64, msg.len() as u64);
    }
}

fn sayln(s: &[u8]) {
    say(s);
    say(b"\n");
}

/// One completed input line (without the newline). 0 = nothing yet.
fn read_line(buf: &mut [u8]) -> usize {
    unsafe { syscall3(SYS_READ, 0, buf.as_mut_ptr() as u64, buf.len() as u64) as usize }
}

/// Print `v` in decimal (no newline, no prefix).
fn say_dec(v: u64) {
    if v == 0 {
        say(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = v;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    say(&buf[i..]);
}

/// Parse a decimal u64 (`kill <pid>` argument). None on any non-digit.
fn parse_u64(b: &[u8]) -> Option<u64> {
    if b.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

/// Split `line` into the command word and the remainder.
fn split_cmd(line: &[u8]) -> (&[u8], &[u8]) {
    let start = line.iter().position(|&b| b != b' ').unwrap_or(line.len());
    let rest_all = &line[start..];
    match rest_all.iter().position(|&b| b == b' ') {
        Some(sp) => {
            let mut rest = &rest_all[sp..];
            while let Some(&b' ') = rest.first() {
                rest = &rest[1..];
            }
            (&rest_all[..sp], rest)
        }
        None => (rest_all, &[]),
    }
}

fn eq(a: &[u8], b: &[u8]) -> bool {
    a == b
}

/// Split `rest` on the first run of spaces into two operands (`ren`).
fn split2(b: &[u8]) -> (&[u8], &[u8]) {
    let (a, rest) = split_cmd(b);
    let start = rest.iter().position(|&x| x != b' ').unwrap_or(rest.len());
    (a, &rest[start..])
}

/// A small output builder: the shell composes multi-part lines (path + size +
/// type) and each `say()` is its own SYS_WRITE, so a preemption between the
/// parts used to interleave another task's output *inside* the line (the
/// `stat:` marker check in test-fs.ps1 is sensitive to exactly that). Building
/// the line first and writing it once makes every line atomic.
struct LineBuf {
    buf: [u8; 160],
    len: usize,
}

impl LineBuf {
    fn new() -> Self {
        Self {
            buf: [0u8; 160],
            len: 0,
        }
    }
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
    }
    /// Append `v` in decimal.
    fn push_dec(&mut self, v: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        let mut v = v;
        if v == 0 {
            digits[19] = b'0';
            i = 19;
        }
        while v > 0 {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.push(&digits[i..]);
    }
    fn flush(&self) {
        say(&self.buf[..self.len]);
    }
}

/// stat: print size + directory flag for `path` (SYS_STAT -> 16-byte record).
fn stat_path(path: &[u8]) {
    let mut st = [0u8; 16];
    let r = unsafe {
        syscall3(
            SYS_STAT,
            path.as_ptr() as u64,
            path.len() as u64,
            st.as_mut_ptr() as u64,
        )
    };
    let mut line = LineBuf::new();
    line.push(b"stat: ");
    line.push(path);
    if is_err(r) {
        if r == (0u64).wrapping_sub(ENOENT) {
            line.push(b": no such file\n");
        } else {
            line.push(b": failed\n");
        }
        line.flush();
        return;
    }
    line.push(b" size=");
    line.push_dec(u64::from_le_bytes([
        st[0], st[1], st[2], st[3], st[4], st[5], st[6], st[7],
    ]));
    line.push(if st[8] != 0 { b" dir\n" } else { b" file\n" });
    line.flush();
}

/// rm: remove the file at `path` (directories are rejected).
fn rm_file(path: &[u8]) {
    let r = unsafe { syscall3(SYS_UNLINK, path.as_ptr() as u64, path.len() as u64, 0) };
    if r == 0 {
        sayln(b"rm: ok");
    } else if r == (0u64).wrapping_sub(ENOENT) {
        sayln(b"rm: no such file");
    } else {
        sayln(b"rm: failed (is it a directory?)");
    }
}

/// ren: rename `from` to `to` (same directory). 4th arg rides in r8.
fn ren_file(from: &[u8], to: &[u8]) {
    let r = unsafe {
        syscall4(
            SYS_RENAME,
            from.as_ptr() as u64, from.len() as u64,
            to.as_ptr() as u64, to.len() as u64,
        )
    };
    if r == 0 {
        sayln(b"ren: ok");
    } else {
        sayln(b"ren: failed (missing, target exists, or cross-dir)");
    }
}

/// Execute one command line.
fn execute(line: &[u8], from_script: bool) {
    let (cmd, rest) = split_cmd(line);
    if cmd.is_empty() {
        return;
    }
    if from_script {
        say(b"shell [autoexec]: ");
        sayln(line);
    }

    if eq(cmd, b"help") {
        sayln(b"commands: ls [path] | mkdir <p> | cat <f> | write <f> <text> |");
        sayln(b"          run <elf> | kill <pid> | echo <text> | sync | lspci | perf |");
        sayln(b"          stat <f> | rm <f> | ren <from> <to> | help | exit");
    } else if eq(cmd, b"stat") {
        if rest.is_empty() {
            sayln(b"usage: stat <path>");
            return;
        }
        stat_path(rest);
    } else if eq(cmd, b"rm") {
        if rest.is_empty() {
            sayln(b"usage: rm <path>");
            return;
        }
        rm_file(rest);
    } else if eq(cmd, b"ren") {
        let (from, to) = split2(rest);
        if from.is_empty() || to.is_empty() {
            sayln(b"usage: ren <from> <to>");
            return;
        }
        ren_file(from, to);
    } else if eq(cmd, b"sync") {
        let r = unsafe { syscall3(SYS_FLUSH, 0, 0, 0) };
        sayln(if r == 0 { b"sync: ok" } else { b"sync: failed" });
    } else if eq(cmd, b"lspci") {
        unsafe {
            let _ = syscall3(SYS_LSPCI, 0, 0, 0);
        }
    } else if eq(cmd, b"perf") {
        unsafe {
            let _ = syscall3(SYS_PERF, 0, 0, 0);
        }
    } else if eq(cmd, b"echo") {
        say(rest);
        say(b"\n");
    } else if eq(cmd, b"ls") {
        let r = if rest.is_empty() {
            unsafe { syscall3(SYS_LS, 0, 0, 0) }
        } else {
            unsafe { syscall3(SYS_LS, rest.as_ptr() as u64, rest.len() as u64, 0) }
        };
        if is_err(r) {
            sayln(b"ls: failed");
        }
    } else if eq(cmd, b"mkdir") {
        if rest.is_empty() {
            sayln(b"usage: mkdir <path>");
            return;
        }
        let r = unsafe { syscall3(SYS_MKDIR, rest.as_ptr() as u64, rest.len() as u64, 0) };
        if is_err(r) {
            sayln(b"mkdir: failed (exists or bad path)");
        } else {
            sayln(b"mkdir: ok");
        }
    } else if eq(cmd, b"cat") {
        if rest.is_empty() {
            sayln(b"usage: cat <file>");
            return;
        }
        cat(rest);
    } else if eq(cmd, b"write") {
        let (path, text) = split_cmd(rest);
        if path.is_empty() {
            sayln(b"usage: write <file> <text>");
            return;
        }
        write_file(path, text);
    } else if eq(cmd, b"run") {
        if rest.is_empty() {
            sayln(b"usage: run <elf>");
            return;
        }
        let r = unsafe { syscall3(SYS_SPAWN, rest.as_ptr() as u64, rest.len() as u64, 0) };
        if is_err(r) {
            sayln(b"run: failed (missing, bad ELF, or region conflict)");
        } else {
            say(b"run: pid ");
            say_dec(r);
            say(b"\n");
            // B3: wait for the child and report its exit status (waitpid).
            let w = unsafe { syscall3(SYS_WAITPID, r, 0, 0) };
            if is_err(w) {
                sayln(b"run: waitpid failed");
            } else {
                say(b"run: exit code ");
                say_dec(w & 0xFFFF_FFFF);
                say(b"\n");
            }
        }
    } else if eq(cmd, b"kill") {
        match parse_u64(rest) {
            None => sayln(b"usage: kill <pid>"),
            Some(pid) => {
                let r = unsafe { syscall3(SYS_KILL, pid, 0, 0) };
                sayln(if r == 0 {
                    b"kill: ok"
                } else {
                    b"kill: failed (no such live pid)"
                });
            }
        }
    } else if eq(cmd, b"exit") {
        sayln(b"shell: bye");
        unsafe { syscall3(SYS_EXIT, 0, 0, 0) };
    } else {
        say(b"shell: unknown command '");
        say(cmd);
        sayln(b"' (try help)");
    }
}

/// cat: open, stream to the console, close. SYS_OPEN creates-if-missing, so
/// a missing file reads back as empty. A newline is printed after the
/// content so output stays line-aligned (files may lack trailing newlines).
fn cat(path: &[u8]) {
    unsafe {
        let fd = syscall3(SYS_OPEN, path.as_ptr() as u64, path.len() as u64, 0);
        if is_err(fd) {
            sayln(b"cat: open failed");
            return;
        }
        let mut buf = [0u8; 256];
        let mut any = false;
        loop {
            let n = syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64);
            if n == 0 || is_err(n) {
                break;
            }
            any = true;
            let _ = syscall3(SYS_WRITE, FD_CONSOLE, buf.as_ptr() as u64, n);
        }
        let _ = syscall3(SYS_CLOSE, fd, 0, 0);
        if any {
            say(b"\n");
        }
    }
}

/// write: create/overwrite the file with `text` (one line, no newline).
fn write_file(path: &[u8], text: &[u8]) {
    unsafe {
        let fd = syscall3(SYS_OPEN, path.as_ptr() as u64, path.len() as u64, 0);
        if is_err(fd) {
            sayln(b"write: open failed (bad path?)");
            return;
        }
        let n = syscall3(SYS_WRITE, fd, text.as_ptr() as u64, text.len() as u64);
        let _ = syscall3(SYS_CLOSE, fd, 0, 0);
        if n == text.len() as u64 {
            sayln(b"write: ok");
        } else {
            sayln(b"write: short write");
        }
    }
}

/// Execute every line of `/AUTOEXEC.TXT` (the headless-test entry point).
fn run_autoexec() {
    unsafe {
        let fd = syscall3(SYS_OPEN, AUTOEXEC.as_ptr() as u64, AUTOEXEC.len() as u64, 0);
        if is_err(fd) {
            sayln(b"shell: no /AUTOEXEC.TXT - interactive mode");
            return;
        }
        let mut data = [0u8; 2048];
        let mut total = 0usize;
        loop {
            if total >= data.len() {
                break;
            }
            let n = syscall3(
                SYS_READ,
                fd,
                data.as_mut_ptr().add(total) as u64,
                (data.len() - total) as u64,
            );
            if n == 0 || is_err(n) {
                break;
            }
            total += n as usize;
        }
        let _ = syscall3(SYS_CLOSE, fd, 0, 0);

        let mut start = 0usize;
        for i in 0..=total {
            if i == total || data[i] == b'\n' {
                let mut line = &data[start..i];
                if line.last() == Some(&b'\r') {
                    line = &line[..line.len() - 1];
                }
                execute(line, true);
                start = i + 1;
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    sayln(b"shell: OnyxOS shell (M8) loaded from disk - type help");
    run_autoexec();

    let mut line_buf = [0u8; 128];
    loop {
        say(b"onyx> ");
        // Poll for a completed line; 0 = nothing yet (other tasks keep
        // running between polls - preemption happens with interrupts on).
        loop {
            let n = read_line(&mut line_buf);
            if n > 0 && n != usize::MAX {
                execute(&line_buf[..n], false);
                break;
            }
        }
    }
}

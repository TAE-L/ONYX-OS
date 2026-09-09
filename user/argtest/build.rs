//! Link `argtest` at a fixed base distinct from `hello` (0x400000), `fstest`
//! (0x800000), `shell` (0xC00000) and `evtest` (0x1000000): user programs
//! share one address space until per-process CR3s exist, so each concurrent
//! program needs its own region. `--no-pie` + `-static` are required: a
//! static-PIE binary ships `.rela.dyn`/`.got`, and the kernel's ELF loader
//! does not apply relocations — the first GOT-based call would jump to 0.
//! 0x1400000 (20 MiB) sits inside the 256 MiB reserved region.
fn main() {
    println!("cargo:rustc-link-arg=--image-base=0x1400000");
    println!("cargo:rustc-link-arg=--no-pie");
    println!("cargo:rustc-link-arg=-static");
}
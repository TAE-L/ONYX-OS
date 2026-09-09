//! Link `evtest` at a fixed base distinct from `hello` (0x400000), `fstest`
//! (0x800000) and `shell` (0xC00000): user programs share one address space
//! until per-process CR3s exist, so each concurrent program needs its own
//! region. `--no-pie` + `-static` are required: a static-PIE binary ships
//! `.rela.dyn`/`.got`, and the kernel's ELF loader does not apply relocations
//! — the first GOT-based call would jump to 0 (observed as a user #PF at
//! RIP=0). 0x1000000 (16 MiB) sits inside the 256 MiB reserved region, clear
//! of the other three bases.
fn main() {
    println!("cargo:rustc-link-arg=--image-base=0x1000000");
    println!("cargo:rustc-link-arg=--no-pie");
    println!("cargo:rustc-link-arg=-static");
}
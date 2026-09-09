//! Link `hello` as a static, non-PIE binary at the fixed virtual base the
//! kernel expects. The kernel's ELF loader maps PT_LOAD segments wherever the
//! file says, but a fixed, predictable base keeps the layout below the user
//! stack region (see kernel/src/userspace.rs for the reserved ranges).
fn main() {
    println!("cargo:rustc-link-arg=--image-base=0x400000");
    println!("cargo:rustc-link-arg=--no-pie");
    println!("cargo:rustc-link-arg=-static");
}
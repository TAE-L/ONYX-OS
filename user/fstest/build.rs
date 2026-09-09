//! Link `fstest` at a fixed base distinct from `hello` (0x400000): both user
//! programs live in the single shared ring-3 address space (M4 state), so
//! their PT_LOAD segments must not overlap.
fn main() {
    println!("cargo:rustc-link-arg=--image-base=0x800000");
    println!("cargo:rustc-link-arg=--no-pie");
    println!("cargo:rustc-link-arg=-static");
}
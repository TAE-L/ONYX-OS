//! No-op build script.
//!
//! The user-hello ELF path does NOT go through here: cargo exposes artifact
//! (`bindeps`) environment variables to the depending crate's rustc for
//! regular `[dependencies]` (only `[build-dependencies]` artifacts are
//! visible to build scripts). kernel/src/userspace.rs therefore reads
//! `CARGO_BIN_FILE_USER_HELLO_hello` directly via `env!()`.
fn main() {}
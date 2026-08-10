//! Shared primitives used by both the `lsp` CLI binary and the bundled
//! Rust-native language servers under `src/servers/`.
//!
//! Everything else in this crate lives in `src/main.rs`'s module tree,
//! which the server binaries can't reach — they're separate `[[bin]]`
//! targets with their own crate roots. This library exists specifically so
//! the handful of things *both* sides need are defined once:
//!
//! - `text_pos`: byte / `char` / UTF-16 position arithmetic. The client
//!   side used to approximate LSP columns as `char` counts while each
//!   bundled server carried its own correct copy of the real conversion.
//! - `uri`: `file:` URI encoding and decoding, previously hand-rolled
//!   (and unescaped) at every call site.
//!
//! Keep this small and dependency-light. It is compiled into five separate
//! binaries.

pub mod text_pos;
pub mod uri;

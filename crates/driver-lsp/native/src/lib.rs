//! Desktop tokio-backed LSP worker.
//!
//! M16 is built in pieces:
//!
//! - [`framing`] — pure Content-Length JSON-RPC framing, no I/O.
//!   Exhaustively unit-tested.
//! - [Future] subprocess manager, per-language spawn, stdin / stdout
//!   pumps, response-id correlation, notification routing, and the
//!   `DiagnosticSource` event loop. Lands incrementally.
//!
//! The sync-side ABI (`LspCmd`, `LspEvent`, `LspDriver`) and the
//! `DiagnosticSource` state machine live in
//! [`led_driver_lsp_core`] — this crate only adds the async parts.

pub mod classify;
pub mod framing;

pub use classify::{Incoming, JsonRpcError, RequestId, classify};
pub use framing::{FrameError, encode_frame, try_parse_frame};

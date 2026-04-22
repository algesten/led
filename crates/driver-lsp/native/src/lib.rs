//! Desktop LSP worker. Same `std::thread` + `std::sync::mpsc`
//! pattern as the rest of the rewrite's `*-native` crates — no
//! tokio, no async runtime. Subprocess I/O blocks per-thread;
//! reader + writer threads per server feed a central mpsc the
//! manager thread drains.
//!
//! M16 is built in pieces:
//!
//! - [`framing`] — pure Content-Length JSON-RPC framing. No I/O.
//! - [`classify`] — pure classifier over decoded frame bodies
//!   (response / server-request / notification / malformed).
//! - [Future] subprocess spawn, stdin / stdout pumps, server
//!   registry, `DiagnosticSource` event loop. Land incrementally.
//!
//! The sync-side ABI (`LspCmd`, `LspEvent`, `LspDriver`) and the
//! `DiagnosticSource` state machine live in
//! [`led_driver_lsp_core`]; this crate adds the platform wiring.

pub mod classify;
pub mod framing;
pub mod protocol;
pub mod registry;
pub mod subprocess;

pub use classify::{Incoming, JsonRpcError, RequestId, classify};
pub use framing::{FrameError, encode_frame, try_parse_frame};
pub use protocol::{
    InitializeCapabilities, build_initialize_request, build_initialized_notification,
    language_id, parse_initialize_response, path_from_uri, uri_from_path,
};
pub use registry::{LspRegistry, ServerConfig};
pub use subprocess::{Server, ServerIncoming, reader_loop, spawn, writer_loop};

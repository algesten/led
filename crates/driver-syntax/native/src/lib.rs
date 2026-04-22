//! Desktop tree-sitter worker — stage 2 fills this in.
//!
//! For stage 1 we only publish a lifecycle marker so the workspace
//! compiles with the core ABI in place. `spawn` and the worker
//! loop land alongside the actual parser wiring.

/// Lifecycle marker — drops when the driver does; the worker
/// self-exits on hangup once it exists.
pub struct SyntaxNative {
    _marker: (),
}

impl SyntaxNative {
    /// Placeholder so stage-1 callers can construct a
    /// `SyntaxNative` without a live worker. Removed when stage 2
    /// lands the real `spawn`.
    pub fn placeholder() -> Self {
        Self { _marker: () }
    }
}

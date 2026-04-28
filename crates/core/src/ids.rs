//! Strongly-typed identifier and value newtypes.
//!
//! All semantically-loaded primitives in led — identifiers,
//! version coordinates, sequence numbers, hashes — are tagged
//! newtypes, never raw integers or strings. Two macros produce
//! them:
//!
//! - [`id_newtype!`] wraps a primitive (default `u64`; pass an
//!   inner type as the second arg for `i64` etc).
//! - [`string_newtype!`] wraps a `String`.
//!
//! ```
//! led_core::id_newtype!(TabId);
//! let a = TabId(1);
//! let b = TabId(2);
//! assert_ne!(a, b);
//! ```

#[macro_export]
macro_rules! id_newtype {
    ($name:ident) => {
        $crate::id_newtype!($name, u64);
    };
    ($name:ident, $inner:ty) => {
        #[derive(
            ::core::marker::Copy,
            ::core::clone::Clone,
            ::core::fmt::Debug,
            ::core::cmp::Eq,
            ::core::cmp::PartialEq,
            ::core::cmp::Ord,
            ::core::cmp::PartialOrd,
            ::core::hash::Hash,
            ::core::default::Default,
            ::serde::Serialize,
            ::serde::Deserialize,
            $crate::drv::Input,
        )]
        #[serde(transparent)]
        pub struct $name(pub $inner);

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}({})", ::core::stringify!($name), self.0)
            }
        }

        impl ::core::convert::From<$inner> for $name {
            fn from(v: $inner) -> Self {
                Self(v)
            }
        }

        impl ::core::convert::From<$name> for $inner {
            fn from(v: $name) -> $inner {
                v.0
            }
        }
    };
}

#[macro_export]
macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(
            ::core::clone::Clone,
            ::core::fmt::Debug,
            ::core::default::Default,
            ::core::cmp::Eq,
            ::core::cmp::PartialEq,
            ::core::cmp::Ord,
            ::core::cmp::PartialOrd,
            ::core::hash::Hash,
            ::serde::Serialize,
            ::serde::Deserialize,
            $crate::drv::Input,
        )]
        #[serde(transparent)]
        pub struct $name(pub ::std::string::String);

        impl $name {
            pub fn new(s: impl ::core::convert::Into<::std::string::String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> ::std::string::String {
                self.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl ::core::convert::From<::std::string::String> for $name {
            fn from(v: ::std::string::String) -> Self {
                Self(v)
            }
        }

        impl ::core::convert::From<&str> for $name {
            fn from(v: &str) -> Self {
                Self(v.to_string())
            }
        }

        impl ::core::convert::From<$name> for ::std::string::String {
            fn from(v: $name) -> ::std::string::String {
                v.0
            }
        }

        impl ::core::convert::AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

// ---- Buffer-edit version coordinates -------------------------------
//
// The pair `(version, saved_version)` is the per-buffer LSP-sync
// state: `version` advances on every edit, `saved_version` advances
// only on disk write. Tracked separately so a save tick (no new
// edit) still triggers `didSave` and reaches rust-analyzer.

id_newtype!(BufferVersion);
id_newtype!(SavedVersion);

// ---- Edit sequence -------------------------------------------------
//
// Monotonic session-wide counter on `EditGroup`. File-search uses
// the same counter to record an undo floor when its overlay opens.

id_newtype!(EditSeq);

// ---- LSP-sync sums -------------------------------------------------
//
// `BufferStateSum` is Σ(saved_version) across all edited buffers
// (the value the runtime stores in `lsp_requested_state_sum` to
// decide when to fire the next `RequestDiagnostics`).

id_newtype!(BufferStateSum);

// ---- LSP request correlation --------------------------------------
//
// The single sequence space used to match RPC responses with the
// requests that issued them — completions, hover, signature-help,
// code-actions, rename, goto-def, references, inlay hints.

id_newtype!(LspRequestSeq);

// ---- File-watch identity ------------------------------------------
//
// Allocated by the runtime when a driver registers a glob; threaded
// back through file-watch events so the runtime knows which
// registration produced an event.

id_newtype!(WatchSeq);

// ---- Undo SQLite ROWID --------------------------------------------
//
// Inner type is `i64` because that is sqlite's ROWID type. Returned
// from the session driver after a successful flush.

id_newtype!(UndoDbSeq, i64);

// ---- Session-undo chain id ----------------------------------------
//
// UUID-like identifier of a per-buffer-session undo chain. Generated
// on first flush per buffer; persisted by the session driver.

string_newtype!(ChainId);

// ---- LSP server id -------------------------------------------------
//
// Logical id of an LSP server (e.g. "rust-analyzer", "pyright").
// Same string the LSP driver uses to demux events back into
// per-server state.

string_newtype!(ServerId);

#[cfg(test)]
mod tests {
    id_newtype!(TestId);
    id_newtype!(OtherId);
    id_newtype!(SignedId, i64);
    string_newtype!(TestName);

    #[test]
    fn distinct_types_do_not_mix() {
        let a = TestId(1);
        let b = OtherId(1);
        // This would fail to compile — proving the newtypes are distinct:
        //   assert_eq!(a, b);
        assert_eq!(a.0, b.0);
    }

    #[test]
    fn display_includes_type_name() {
        assert_eq!(TestId(42).to_string(), "TestId(42)");
    }

    #[test]
    fn from_u64_and_back() {
        let id: TestId = 7u64.into();
        let n: u64 = id.into();
        assert_eq!(n, 7);
    }

    #[test]
    fn signed_inner_type_works() {
        let id = SignedId(-5);
        let n: i64 = id.into();
        assert_eq!(n, -5);
    }

    #[test]
    fn string_newtype_round_trip() {
        let n = TestName::new("foo");
        assert_eq!(n.as_str(), "foo");
        let s: String = n.into();
        assert_eq!(s, "foo");
    }

    #[test]
    fn string_newtype_from_str_literal() {
        let n: TestName = "bar".into();
        assert_eq!(n.as_str(), "bar");
    }
}

//! Strongly-typed identifier newtypes.
//!
//! All identifiers in led are newtypes around `u64` — never raw integers.
//! The [`id_newtype!`](crate::id_newtype) macro defines one.
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
        )]
        pub struct $name(pub u64);

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}({})", ::core::stringify!($name), self.0)
            }
        }

        impl ::core::convert::From<u64> for $name {
            fn from(v: u64) -> Self {
                Self(v)
            }
        }

        impl ::core::convert::From<$name> for u64 {
            fn from(v: $name) -> u64 {
                v.0
            }
        }

        // Hand-rolled `ToStatic` impl so the newtype can be
        // used directly as a `#[drv::memo]` input. drv's
        // `#[derive(drv::Input)]` requires named fields, so
        // tuple-struct id newtypes can't use the derive —
        // luckily the impl is trivial for a `Copy` newtype
        // around `u64`.
        impl $crate::drv::ToStatic for $name {
            type Static = $name;
            fn to_static(&self) -> Self::Static { *self }
            fn eq_static(&self, other: &Self::Static) -> bool {
                self == other
            }
        }
    };
}

#[cfg(test)]
mod tests {
    id_newtype!(TestId);
    id_newtype!(OtherId);

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
}

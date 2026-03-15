use std::ops::Deref;

/// A value paired with a version counter. Each call to `set` increments the version,
/// making it easy to detect changes via `dedupe_by(|s| s.field.version())` in
/// derived streams.
#[derive(Debug, Clone)]
pub struct Versioned<T> {
    value: T,
    version: u64,
}

impl<T> Versioned<T> {
    pub fn new(value: T) -> Self {
        Self { value, version: 0 }
    }

    pub fn set(&mut self, value: T) {
        self.value = value;
        self.version += 1;
    }

    pub fn version(&self) -> u64 {
        self.version
    }
}

impl<T: Default> Default for Versioned<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> Deref for Versioned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

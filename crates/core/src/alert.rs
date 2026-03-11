use std::fmt;

#[derive(Debug, Clone)]
pub enum Alert {
    Info(String),
    Warn(String),
}

impl fmt::Display for Alert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Alert::Info(v) => write!(f, "info: {}", v),
            Alert::Warn(v) => write!(f, "warn: {}", v),
        }
    }
}

pub trait AlertExt<T> {
    fn as_info(self) -> Result<T, Alert>;
    fn as_warn(self) -> Result<T, Alert>;
}

impl<T, E: std::error::Error> AlertExt<T> for Result<T, E> {
    fn as_info(self) -> Result<T, Alert> {
        self.map_err(|e| Alert::Info(e.to_string()))
    }

    fn as_warn(self) -> Result<T, Alert> {
        self.map_err(|e| Alert::Warn(e.to_string()))
    }
}

use std::sync::{Arc, Mutex};
use std::time::Instant;

use led_core::logging::{LogBuffer, LogEntry, SharedLog};

struct AppLogger {
    shared: SharedLog,
    start: Instant,
}

impl log::Log for AppLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let entry = LogEntry {
            elapsed: self.start.elapsed(),
            level: record.level(),
            message: format!("{}", record.args()),
        };
        if let Ok(mut buf) = self.shared.lock() {
            buf.push(entry);
        }
    }

    fn flush(&self) {}
}

pub fn init(level: log::LevelFilter) -> SharedLog {
    let shared: SharedLog = Arc::new(Mutex::new(LogBuffer::new(10_000)));
    let logger = AppLogger {
        shared: shared.clone(),
        start: Instant::now(),
    };
    // Box::leak is the standard pattern for log::set_logger
    log::set_logger(Box::leak(Box::new(logger)))
        .map(|()| log::set_max_level(level))
        .expect("logger already initialized");
    shared
}

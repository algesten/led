use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use led_core::logging::{LogBuffer, LogEntry, SharedLog};

type LogFile = Arc<Mutex<std::io::BufWriter<std::fs::File>>>;

struct AppLogger {
    shared: SharedLog,
    start: Instant,
    file: Option<LogFile>,
}

impl log::Log for AppLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let elapsed = self.start.elapsed();
        let message = format!("{}", record.args());

        let entry = LogEntry {
            elapsed,
            level: record.level(),
            message: message.clone(),
        };
        if let Ok(mut buf) = self.shared.lock() {
            buf.push(entry);
        }

        if let Some(ref file) = self.file {
            if let Ok(mut f) = file.lock() {
                let secs = elapsed.as_secs_f64();
                let _ = writeln!(f, "[{secs:>10.3}] {:<5} {message}", record.level());
                let _ = f.flush();
            }
        }
    }

    fn flush(&self) {}
}

pub fn init(level: log::LevelFilter, log_file: Option<&str>) -> SharedLog {
    let shared: SharedLog = Arc::new(Mutex::new(LogBuffer::new(10_000)));

    let file: Option<LogFile> = log_file.and_then(|path| {
        std::fs::File::create(path)
            .ok()
            .map(|f| Arc::new(Mutex::new(std::io::BufWriter::new(f))))
    });

    let logger = AppLogger {
        shared: shared.clone(),
        start: Instant::now(),
        file,
    };
    // Box::leak is the standard pattern for log::set_logger
    log::set_logger(Box::leak(Box::new(logger)))
        .map(|()| log::set_max_level(level))
        .expect("logger already initialized");
    shared
}

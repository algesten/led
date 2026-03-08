use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub struct LogEntry {
    pub elapsed: std::time::Duration,
    pub level: log::Level,
    pub message: String,
}

pub struct LogBuffer {
    entries: VecDeque<LogEntry>,
    total_pushed: usize,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(1024)),
            total_pushed: 0,
            capacity,
        }
    }

    pub fn push(&mut self, entry: LogEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.total_pushed += 1;
    }

    pub fn total_pushed(&self) -> usize {
        self.total_pushed
    }

    pub fn entries(&self) -> &VecDeque<LogEntry> {
        &self.entries
    }
}

pub type SharedLog = Arc<Mutex<LogBuffer>>;

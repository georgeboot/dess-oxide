//! The last few thousand log lines, kept in memory for the debug download
//! (see [`crate::debug`]). Everything still goes to stderr as well, where
//! Home Assistant's log picks it up.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Mutex, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

/// About a day of a quiet service's log.
const KEEP: usize = 5000;

static LINES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Writes each log event to stderr and keeps a copy of its lines.
pub struct Tee;

impl<'a> MakeWriter<'a> for Tee {
    type Writer = Event;

    fn make_writer(&'a self) -> Event {
        Event(Vec::new())
    }
}

/// One event's output, written out and kept when it's dropped.
pub struct Event(Vec<u8>);

impl Write for Event {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = io::stderr().write_all(&self.0);
        keep(&String::from_utf8_lossy(&self.0));
    }
}

fn keep(text: &str) {
    let mut lines = LINES.lock().unwrap_or_else(PoisonError::into_inner);
    for line in text.lines() {
        if lines.len() == KEEP {
            lines.pop_front();
        }
        lines.push_back(line.to_owned());
    }
}

/// The kept lines, oldest first.
pub fn recent() -> Vec<String> {
    LINES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_latest_lines() {
        for i in 0..KEEP + 10 {
            keep(&format!("line {i}\n"));
        }
        let lines = recent();
        assert_eq!(lines.len(), KEEP);
        assert_eq!(lines.last().unwrap(), &format!("line {}", KEEP + 9));
    }
}

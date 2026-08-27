use std::{collections::VecDeque, sync::OnceLock};

use chrono::{DateTime, Local};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogKind {
    Output,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub sequence: u64,
    pub timestamp: DateTime<Local>,
    pub text: String,
    pub kind: LogKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogMetadata {
    pub count: usize,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogUpdate {
    pub metadata: LogMetadata,
    pub lines: Vec<LogLine>,
}

impl LogLine {
    pub fn display(&self, timestamps: bool) -> String {
        let prefix = if timestamps {
            format!("{} ", self.timestamp.format("%H:%M:%S%.3f"))
        } else {
            String::new()
        };
        match self.kind {
            LogKind::Output => format!("{prefix}{}", self.text),
            LogKind::System => format!("{prefix}[blade] {}", self.text),
        }
    }

    pub fn file_display(&self) -> String {
        format!(
            "{} {}{}",
            self.timestamp.format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
            if self.kind == LogKind::System {
                "[blade] "
            } else {
                ""
            },
            self.text
        )
    }
}

#[derive(Debug)]
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
    next_sequence: u64,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
            next_sequence: 0,
        }
    }

    pub fn push(&mut self, kind: LogKind, text: impl Into<String>) -> LogLine {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        let line = LogLine {
            sequence: self.next_sequence,
            timestamp: Local::now(),
            text: sanitize_terminal_text(&text.into()),
            kind,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.lines.push_back(line.clone());
        line
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.lines.iter().cloned().collect()
    }

    pub fn metadata(&self) -> LogMetadata {
        LogMetadata {
            count: self.lines.len(),
            first_sequence: self.lines.front().map(|line| line.sequence),
            last_sequence: self.lines.back().map(|line| line.sequence),
        }
    }

    pub fn update_since(&self, after: Option<u64>) -> LogUpdate {
        let lines = self
            .lines
            .iter()
            .filter(|line| after.is_none_or(|sequence| line.sequence > sequence))
            .cloned()
            .collect();
        LogUpdate {
            metadata: self.metadata(),
            lines,
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

pub fn sanitize_terminal_text(text: &str) -> String {
    static ANSI_ESCAPE: OnceLock<Regex> = OnceLock::new();
    let regex = ANSI_ESCAPE.get_or_init(|| {
        Regex::new(r"(?:\x1B\[[0-?]*[ -/]*[@-~])|(?:\x1B\][^\x07]*(?:\x07|\x1B\\))")
            .expect("the ANSI escape regex is valid")
    });
    let without_ansi = regex.replace_all(text, "");
    without_ansi
        .chars()
        .filter(|character| *character == '\t' || !character.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{LogBuffer, LogKind, sanitize_terminal_text};

    #[test]
    fn evicts_oldest_lines_at_capacity() {
        let mut buffer = LogBuffer::new(2);
        buffer.push(LogKind::Output, "one");
        buffer.push(LogKind::Output, "two");
        buffer.push(LogKind::Output, "three");
        let lines = buffer.snapshot();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "two");
        assert_eq!(lines[1].text, "three");
    }

    #[test]
    fn removes_ansi_sequences_and_control_characters() {
        assert_eq!(sanitize_terminal_text("\x1b[31mred\x1b[0m\0"), "red");
    }

    #[test]
    fn clearing_preserves_the_sequence_for_new_output() {
        let mut buffer = LogBuffer::new(2);
        let first = buffer.push(LogKind::Output, "before");

        buffer.clear();

        assert!(buffer.snapshot().is_empty());
        let second = buffer.push(LogKind::Output, "after");
        assert!(second.sequence > first.sequence);
    }

    #[test]
    fn incremental_updates_include_only_new_lines_and_current_metadata() {
        let mut buffer = LogBuffer::new(3);
        let first = buffer.push(LogKind::Output, "one");
        let second = buffer.push(LogKind::Output, "two");

        let initial = buffer.update_since(None);
        assert_eq!(initial.metadata.count, 2);
        assert_eq!(initial.metadata.first_sequence, Some(first.sequence));
        assert_eq!(initial.metadata.last_sequence, Some(second.sequence));
        assert_eq!(initial.lines.len(), 2);

        buffer.push(LogKind::Output, "three");
        let fourth = buffer.push(LogKind::Output, "four");
        let update = buffer.update_since(Some(second.sequence));
        assert_eq!(update.metadata.count, 3);
        assert_eq!(update.metadata.first_sequence, Some(second.sequence));
        assert_eq!(update.metadata.last_sequence, Some(fourth.sequence));
        assert_eq!(
            update
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["three", "four"]
        );
    }
}

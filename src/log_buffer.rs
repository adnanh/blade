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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "anchor", rename_all = "kebab-case")]
pub enum LogPageAnchor {
    Before { sequence: u64 },
    After { sequence: Option<u64> },
    Around { sequence: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSearchResult {
    pub sequence: u64,
    pub page: LogUpdate,
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

    #[cfg(test)]
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

    pub fn tail_update(&self, limit: usize) -> LogUpdate {
        self.update_since_limited(None, limit)
    }

    pub fn update_since_limited(&self, after: Option<u64>, limit: usize) -> LogUpdate {
        let mut lines = self
            .lines
            .iter()
            .filter(|line| after.is_none_or(|sequence| line.sequence > sequence))
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        lines.reverse();
        LogUpdate {
            metadata: self.metadata(),
            lines,
        }
    }

    pub fn page(&self, anchor: LogPageAnchor, through: Option<u64>, limit: usize) -> LogUpdate {
        let within_snapshot =
            |line: &&LogLine| through.is_none_or(|last_sequence| line.sequence <= last_sequence);
        let lines = match anchor {
            LogPageAnchor::Before { sequence } => {
                let mut lines = self
                    .lines
                    .iter()
                    .filter(within_snapshot)
                    .filter(|line| line.sequence < sequence)
                    .rev()
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>();
                lines.reverse();
                lines
            }
            LogPageAnchor::After { sequence } => self
                .lines
                .iter()
                .filter(within_snapshot)
                .filter(|line| sequence.is_none_or(|sequence| line.sequence > sequence))
                .take(limit)
                .cloned()
                .collect(),
            LogPageAnchor::Around { sequence } => {
                let available_len = through
                    .and_then(|last_sequence| {
                        self.lines
                            .iter()
                            .rposition(|line| line.sequence <= last_sequence)
                            .map(|position| position + 1)
                    })
                    .unwrap_or(self.lines.len());
                let Some(position) = self
                    .lines
                    .iter()
                    .take(available_len)
                    .position(|line| line.sequence == sequence)
                else {
                    return LogUpdate {
                        metadata: self.metadata(),
                        lines: Vec::new(),
                    };
                };
                let start = position
                    .saturating_sub(limit / 2)
                    .min(available_len.saturating_sub(limit));
                self.lines
                    .iter()
                    .skip(start)
                    .take(limit.min(available_len.saturating_sub(start)))
                    .cloned()
                    .collect()
            }
        };
        LogUpdate {
            metadata: self.metadata(),
            lines,
        }
    }

    pub fn search(
        &self,
        query: &str,
        insensitive_regex: bool,
        from: Option<u64>,
        backwards: bool,
        page_limit: usize,
    ) -> Result<Option<LogSearchResult>, regex::Error> {
        let regex = if insensitive_regex {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()?
        } else {
            regex::Regex::new(&regex::escape(query))?
        };
        let found = if backwards {
            self.lines
                .iter()
                .rev()
                .find(|line| {
                    from.is_none_or(|from| line.sequence < from) && regex.is_match(&line.text)
                })
                .or_else(|| {
                    self.lines
                        .iter()
                        .rev()
                        .find(|line| regex.is_match(&line.text))
                })
        } else {
            self.lines
                .iter()
                .find(|line| {
                    from.is_none_or(|from| line.sequence > from) && regex.is_match(&line.text)
                })
                .or_else(|| self.lines.iter().find(|line| regex.is_match(&line.text)))
        };
        Ok(found.map(|line| LogSearchResult {
            sequence: line.sequence,
            page: self.page(
                LogPageAnchor::Around {
                    sequence: line.sequence,
                },
                None,
                page_limit,
            ),
        }))
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
    use super::{LogBuffer, LogKind, LogPageAnchor, sanitize_terminal_text};

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

        let initial = buffer.update_since_limited(None, usize::MAX);
        assert_eq!(initial.metadata.count, 2);
        assert_eq!(initial.metadata.first_sequence, Some(first.sequence));
        assert_eq!(initial.metadata.last_sequence, Some(second.sequence));
        assert_eq!(initial.lines.len(), 2);

        buffer.push(LogKind::Output, "three");
        let fourth = buffer.push(LogKind::Output, "four");
        let update = buffer.update_since_limited(Some(second.sequence), usize::MAX);
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

    #[test]
    fn tail_updates_bound_initial_history_without_losing_metadata() {
        let mut buffer = LogBuffer::new(5);
        for text in ["one", "two", "three", "four"] {
            buffer.push(LogKind::Output, text);
        }

        let update = buffer.tail_update(2);
        assert_eq!(update.metadata.count, 4);
        assert_eq!(
            update
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["three", "four"]
        );

        let third = buffer.snapshot()[2].sequence;
        buffer.push(LogKind::Output, "five");
        buffer.push(LogKind::Output, "six");
        buffer.push(LogKind::Output, "seven");
        let update = buffer.update_since_limited(Some(third), 2);
        assert_eq!(update.metadata.count, 5);
        assert_eq!(
            update
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["six", "seven"]
        );
    }

    #[test]
    fn pages_before_after_and_around_sequence_cursors() {
        let mut buffer = LogBuffer::new(20);
        let sequences = (0..10)
            .map(|index| buffer.push(LogKind::Output, index.to_string()).sequence)
            .collect::<Vec<_>>();

        let before = buffer.page(
            LogPageAnchor::Before {
                sequence: sequences[5],
            },
            None,
            2,
        );
        assert_eq!(
            before
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["3", "4"]
        );

        let after = buffer.page(
            LogPageAnchor::After {
                sequence: Some(sequences[4]),
            },
            Some(sequences[6]),
            5,
        );
        assert_eq!(
            after
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["5", "6"]
        );

        let around = buffer.page(
            LogPageAnchor::Around {
                sequence: sequences[8],
            },
            None,
            5,
        );
        assert_eq!(
            around
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["5", "6", "7", "8", "9"]
        );
    }

    #[test]
    fn searches_wrap_by_sequence_and_return_a_page_around_the_match() {
        let mut buffer = LogBuffer::new(20);
        let first = buffer.push(LogKind::Output, "Needle one");
        let second = buffer.push(LogKind::Output, "other");
        let third = buffer.push(LogKind::Output, "needle two");

        let exact = buffer
            .search("Needle", false, Some(third.sequence), false, 3)
            .unwrap()
            .unwrap();
        assert_eq!(exact.sequence, first.sequence);
        assert!(
            exact
                .page
                .lines
                .iter()
                .any(|line| line.sequence == exact.sequence)
        );

        let backwards = buffer
            .search("n.edle", true, Some(first.sequence), true, 3)
            .unwrap()
            .unwrap();
        assert_eq!(backwards.sequence, third.sequence);
        assert_ne!(backwards.sequence, second.sequence);
    }
}

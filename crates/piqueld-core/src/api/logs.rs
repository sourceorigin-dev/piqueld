//! Container output read from Docker.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::names::string_enum;

/// Bounded historical container output read directly from Docker.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct ApplicationLogs {
    /// Chronologically ordered records.
    pub items: Vec<LogRecord>,
    /// More output existed than the requested window or safety limit.
    pub truncated: bool,
}

string_enum! {
    #[serde]
    /// A selectable process output stream. Merged terminal output is only included without a filter.
    pub enum LogStream {
        /// Standard output.
        Stdout => "stdout",
        /// Standard error.
        Stderr => "stderr",
    }
}

/// One line of workload output with replica identity.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct LogRecord {
    /// Logical service name.
    pub service: String,
    /// Swarm task identity.
    pub task_id: String,
    /// Docker timestamp, when supplied.
    pub timestamp: String,
    /// stdout, stderr, or console.
    pub stream: String,
    /// Text with terminal control sequences removed.
    pub message: String,
}

impl LogRecord {
    /// Removes terminal control sequences, including CSI colors and OSC titles/links.
    /// Other control characters are dropped too, except tabs and newlines.
    ///
    /// ```text
    /// "\x1b[31mred\x1b[0m\r\ttext\n"                -> "red\ttext\n"
    /// "\x1b]0;title\x07text \x1b]8;;url\x1b\\link" -> "text link"
    /// ```
    #[must_use]
    pub fn clean_message(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                match chars.next() {
                    // CSI: parameters run until a final byte in `@..=~`.
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    // OSC, DCS, PM, APC: strings end at BEL or ST (`ESC \`).
                    Some(']' | 'P' | '^' | '_') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    // Other two-byte escapes drop just the escape and its selector.
                    _ => {}
                }
            } else if !ch.is_control() || matches!(ch, '\t' | '\n') {
                output.push(ch);
            }
        }
        output
    }
}

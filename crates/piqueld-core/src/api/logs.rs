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
    /// Whether `character` could hide or reorder the text around it: a
    /// control character, or one of the common invisible Unicode format
    /// characters, such as a bidirectional override (U+202A–U+202E,
    /// U+2066–U+2069), a direction mark (U+200E, U+200F, U+061C), a
    /// zero-width one, a Hangul filler, or the line and paragraph separators
    /// (U+2028, U+2029). `piquelctl` escapes these in its output.
    #[must_use]
    pub const fn hidden(character: char) -> bool {
        character.is_control()
            || matches!(
                character,
                '\u{00AD}'
                    | '\u{034F}'
                    | '\u{061C}'
                    | '\u{115F}'
                    | '\u{1160}'
                    | '\u{180E}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{2028}'..='\u{202E}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{206F}'
                    | '\u{3164}'
                    | '\u{FEFF}'
                    | '\u{FFA0}'
                    | '\u{FFF9}'..='\u{FFFB}'
                    | '\u{E0001}'
                    | '\u{E0020}'..='\u{E007F}'
            )
    }

    /// Removes terminal control sequences, including CSI colors and OSC titles/links.
    /// Other [`Self::hidden`] characters are dropped too, except tabs, newlines,
    /// and the zero-width (non-)joiners that join emoji and scripts.
    ///
    /// ```text
    /// "\x1b[31mred\x1b[0m\r\ttext\n"                -> "red\ttext\n"
    /// "\x1b]0;title\x07text \x1b]8;;url\x1b\\link" -> "text link"
    /// "evil\u{202E}gpj.exe"                     -> "evilgpj.exe"
    /// ```
    #[must_use]
    pub fn clean_message(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                match chars.next() {
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    Some(']' | 'P' | '^' | '_') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            } else if !Self::hidden(ch) || matches!(ch, '\t' | '\n' | '\u{200C}' | '\u{200D}') {
                output.push(ch);
            }
        }
        output
    }
}

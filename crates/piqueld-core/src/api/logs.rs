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
            } else if !ch.is_control() || matches!(ch, '\t' | '\n') {
                output.push(ch);
            }
        }
        output
    }
}

/// A moment output was written, in Unix nanoseconds, as Docker stamps each
/// line: written and read as a UTC RFC 3339 timestamp, such as
/// `2026-01-01T00:00:00.000000001Z`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LogTime(pub i64);

impl LogTime {
    /// Reads a UTC RFC 3339 timestamp; `None` for any other text, or one
    /// out of range.
    #[must_use]
    pub fn parse(timestamp: &str) -> Option<Self> {
        let (date, time) = timestamp.strip_suffix('Z')?.split_once('T')?;
        let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
        // Widen parsed i64 fields before calendar arithmetic, so malformed
        // input cannot overflow before the final nanosecond range check.
        let numbers = |text: &str, separator| -> Option<[i128; 3]> {
            text.split(separator)
                .map(|part| part.parse::<i64>().ok().map(i128::from))
                .collect::<Option<Vec<_>>>()?
                .try_into()
                .ok()
        };
        let [year, month, day] = numbers(date, '-')?;
        let [hour, minute, second] = numbers(clock, ':')?;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || fraction.len() > 9
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let nanos: i128 = format!("{fraction:0<9}").parse().ok()?;
        // Days since 1970-01-01 in the proleptic Gregorian calendar, after
        // Howard Hinnant's `days_from_civil`.
        let year = if month <= 2 { year - 1 } else { year };
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
        i64::try_from(seconds * 1_000_000_000 + nanos)
            .ok()
            .map(Self)
    }
}

impl std::fmt::Display for LogTime {
    /// To the nanosecond, as Docker writes it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds =
            crate::observability::UtcTime(self.0.div_euclid(1_000_000_000) * 1_000).rfc3339();
        // `2026-01-01T00:00:00`, without the milliseconds.
        let seconds = seconds.get(..19).unwrap_or(&seconds);
        write!(
            formatter,
            "{seconds}.{:09}Z",
            self.0.rem_euclid(1_000_000_000)
        )
    }
}

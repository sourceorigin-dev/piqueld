//! Error text with its causes.

use std::error::Error;

/// Renders an error with the causes it keeps, for log lines and the failure
/// texts stored for the API: what `{:#}` prints for an `anyhow::Error`.
pub trait ErrorChain: Error + Sized {
    /// This error and its causes, joined as `outer: inner`. A cause that
    /// prints what the error wrapping it printed, as an enum variant that
    /// only forwards its source does, appears once.
    #[must_use]
    fn chain(&self) -> String {
        let mut texts: Vec<String> = Vec::new();
        for error in std::iter::successors(Some(self as &dyn Error), |error| (*error).source()) {
            let text = error.to_string();
            if texts.last() != Some(&text) {
                texts.push(text);
            }
        }
        texts.join(": ")
    }
}

impl<E: Error> ErrorChain for E {}

#[cfg(test)]
mod tests {
    use super::ErrorChain as _;

    /// A cause kept as `#[source]` appears after the error's own text.
    #[test]
    fn chain_joins_each_cause() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] std::io::Error);
        let error = Outer(std::io::Error::other("inner"));
        assert_eq!(error.chain(), "outer: inner");
    }

    /// A wrapper that restates its source's message adds nothing.
    #[test]
    fn chain_prints_a_repeated_message_once() {
        #[derive(Debug, thiserror::Error)]
        #[error("{0}")]
        struct Forwarding(#[source] std::io::Error);
        let error = Forwarding(std::io::Error::other("inner"));
        assert_eq!(error.chain(), "inner");
    }
}

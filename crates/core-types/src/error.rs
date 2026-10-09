//! [`error_chain`]: an error and its causes on one line.

use std::error::Error;

/// `err`'s message followed by each of its causes', joined by `: `, for a
/// log line or an error that wraps a library's. The caller strips what
/// mustn't show first, such as a request's URL.
///
/// ```
/// use core_types::error_chain;
///
/// assert_eq!(
///     error_chain(&std::fmt::Error),
///     "an error occurred when formatting an argument"
/// );
/// ```
pub fn error_chain(err: &(dyn Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for Layer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.1
                .as_deref()
                .map(|inner| inner as &(dyn Error + 'static))
        }
    }

    #[test]
    fn every_cause_follows_the_message() {
        let err = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "client error (Connect)",
                Some(Box::new(Layer("tcp connect error", None))),
            ))),
        );
        assert_eq!(
            error_chain(&err),
            "error sending request: client error (Connect): tcp connect error"
        );
        assert_eq!(error_chain(&Layer("alone", None)), "alone");
    }
}

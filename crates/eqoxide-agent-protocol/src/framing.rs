//! Newline-delimited JSON (NDJSON) framing over the agent socket. No I/O here — pure
//! encode/decode so the framing logic is testable without a live connection.

/// Serialize `value` to one JSON line, terminated by `\n`. The trailing newline is what lets the
/// reader split frames with a plain line reader instead of a length-prefixed protocol.
pub fn encode_line<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    Ok(s)
}

/// Deserialize one JSON line. `line` may or may not carry the trailing `\n` (tokio's `lines()`
/// strips it; callers reading raw bytes may not have), so both are accepted.
pub fn decode_line<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[test]
    fn encode_line_appends_exactly_one_trailing_newline() {
        let line = encode_line(&Sample { a: 1, b: "x".into() }).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn decode_line_round_trips_with_and_without_trailing_newline() {
        let original = Sample { a: 42, b: "hello".into() };
        let encoded = encode_line(&original).unwrap();
        let decoded: Sample = decode_line(&encoded).unwrap();
        assert_eq!(decoded, original);
        let trimmed: Sample = decode_line(encoded.trim_end_matches('\n')).unwrap();
        assert_eq!(trimmed, original);
    }
}

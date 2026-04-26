//! Pure JSON-RPC framing — [LSP base protocol] length-prefixed
//! frames. No tokio, no I/O: just byte-level encode / parse so
//! the tricky bits (partial headers, wrong content-length,
//! malformed input) are exhaustively unit-testable.
//!
//! [LSP base protocol]: https://microsoft.github.io/language-server-protocol/specifications/base/0.9/specification/#baseProtocol
//!
//! # Wire format
//!
//! A frame is one or more ASCII header lines terminated by
//! `\r\n`, then a blank `\r\n`, then a UTF-8 body of exactly
//! `Content-Length` bytes.
//!
//! ```text
//! Content-Length: 44\r\n
//! Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n
//! \r\n
//! {"jsonrpc":"2.0","method":"initialized",...}
//! ```
//!
//! Only `Content-Length` is mandatory for us; other headers are
//! tolerated and ignored. `Content-Type` is sometimes sent but
//! always defaults to UTF-8 per the spec.

use std::fmt;

/// Wrap a JSON-RPC body in the LSP frame envelope. The caller is
/// expected to pass valid JSON as UTF-8 bytes. This doesn't
/// validate — it just prepends the header.
///
/// Returned as `Vec<u8>` so the caller can hand it to a byte
/// writer without a second copy.
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(body);
    out
}

/// Framing-level parse error. Kept small because the callers
/// (async reader / sync test) both want "did this succeed / do I
/// need more bytes / is the stream corrupt?".
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Header line wasn't valid ASCII UTF-8.
    NonUtf8Header,
    /// `Content-Length` was present but unparseable as `usize`.
    BadContentLength(String),
    /// No `Content-Length` header found before the blank line.
    MissingContentLength,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUtf8Header => write!(f, "non-UTF-8 in header"),
            Self::BadContentLength(s) => write!(f, "bad Content-Length: {s:?}"),
            Self::MissingContentLength => write!(f, "missing Content-Length"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Incremental frame parser. Called repeatedly with an accumulating
/// byte buffer; returns:
///
/// - `Ok(Some((consumed, body)))` — a complete frame was found;
///   the caller drains `consumed` bytes off the front of the
///   buffer and takes ownership of `body`.
/// - `Ok(None)` — buffer doesn't yet contain a complete frame.
///   Caller reads more bytes and re-tries.
/// - `Err(_)` — buffer contains unrecoverable garbage (bad header
///   encoding / malformed Content-Length). Caller should close
///   the connection; the server is broken.
///
/// Works regardless of whether the caller feeds bytes from a
/// `std::io::Read`, a `tokio::io::AsyncRead`, or a fixed
/// `Vec<u8>` in a test — all of the I/O choice stays outside.
pub fn try_parse_frame(buf: &[u8]) -> Result<Option<(usize, Vec<u8>)>, FrameError> {
    // Locate the end-of-headers marker `\r\n\r\n`. If absent,
    // we're still reading headers — return None to request more
    // bytes.
    let Some(header_end) = find_double_crlf(buf) else {
        return Ok(None);
    };
    let headers_bytes = &buf[..header_end];
    let headers = std::str::from_utf8(headers_bytes).map_err(|_| FrameError::NonUtf8Header)?;

    // Parse header lines. We only care about `Content-Length`;
    // everything else is tolerated (and ignored). Comparison is
    // case-insensitive per RFC 822 / HTTP.
    let mut content_length: Option<usize> = None;
    for line in headers.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            let trimmed = value.trim();
            content_length = Some(
                trimmed
                    .parse::<usize>()
                    .map_err(|_| FrameError::BadContentLength(trimmed.to_string()))?,
            );
        }
    }
    let len = content_length.ok_or(FrameError::MissingContentLength)?;

    let body_start = header_end + 4; // skip the \r\n\r\n
    let body_end = body_start + len;
    if buf.len() < body_end {
        // Have the full header, need more body bytes.
        return Ok(None);
    }
    let body = buf[body_start..body_end].to_vec();
    Ok(Some((body_end, body)))
}

/// Find the byte index of the `\r\n\r\n` that terminates the
/// header block, or `None` if no such sequence exists yet.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    for i in 0..=buf.len() - 4 {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> Vec<u8> {
        encode_frame(body.as_bytes())
    }

    // ── encode ──────────────────────────────────────────────

    #[test]
    fn encode_prepends_content_length() {
        let out = encode_frame(b"{}");
        assert_eq!(out, b"Content-Length: 2\r\n\r\n{}");
    }

    #[test]
    fn encode_uses_byte_len_not_char_count() {
        // `é` is two bytes in UTF-8. The header must report 2,
        // not 1 — LSP says bytes.
        let out = encode_frame("é".as_bytes());
        assert_eq!(out, b"Content-Length: 2\r\n\r\n\xc3\xa9");
    }

    #[test]
    fn encode_handles_empty_body() {
        let out = encode_frame(b"");
        assert_eq!(out, b"Content-Length: 0\r\n\r\n");
    }

    // ── incremental parse ──────────────────────────────────

    #[test]
    fn parse_complete_frame_returns_body_and_consumed() {
        let buf = frame(r#"{"jsonrpc":"2.0"}"#);
        let consumed = buf.len();
        let (n, body) = try_parse_frame(&buf).unwrap().unwrap();
        assert_eq!(n, consumed);
        assert_eq!(body, br#"{"jsonrpc":"2.0"}"#);
    }

    #[test]
    fn parse_returns_none_when_headers_incomplete() {
        let partial = b"Content-Length: 10\r\n"; // no blank line yet
        assert_eq!(try_parse_frame(partial).unwrap(), None);
    }

    #[test]
    fn parse_returns_none_when_body_incomplete() {
        let mut buf = b"Content-Length: 10\r\n\r\nhi".to_vec();
        assert_eq!(try_parse_frame(&buf).unwrap(), None);
        buf.extend_from_slice(b"world!!!!");
        let (_, body) = try_parse_frame(&buf).unwrap().unwrap();
        assert_eq!(body, b"hiworld!!!");
    }

    #[test]
    fn parse_ignores_extra_headers() {
        let buf = b"Content-Length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        let (_, body) = try_parse_frame(buf).unwrap().unwrap();
        assert_eq!(body, b"{}");
    }

    #[test]
    fn parse_header_name_is_case_insensitive() {
        let buf = b"content-length: 2\r\n\r\n{}";
        let (_, body) = try_parse_frame(buf).unwrap().unwrap();
        assert_eq!(body, b"{}");
    }

    #[test]
    fn parse_rejects_missing_content_length() {
        let buf = b"X-Other: foo\r\n\r\n{}";
        assert_eq!(
            try_parse_frame(buf).unwrap_err(),
            FrameError::MissingContentLength
        );
    }

    #[test]
    fn parse_rejects_malformed_content_length() {
        let buf = b"Content-Length: abc\r\n\r\n{}";
        match try_parse_frame(buf).unwrap_err() {
            FrameError::BadContentLength(s) => assert_eq!(s, "abc"),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn parse_back_to_back_frames_consume_first_only() {
        // The parser returns how many bytes belong to the first
        // frame; callers use that to trim for the second call.
        let mut buf = frame("{}");
        buf.extend_from_slice(&frame("[]"));
        let (n1, b1) = try_parse_frame(&buf).unwrap().unwrap();
        assert_eq!(b1, b"{}");
        let (_, b2) = try_parse_frame(&buf[n1..]).unwrap().unwrap();
        assert_eq!(b2, b"[]");
    }

    #[test]
    fn parse_zero_length_body_is_well_formed() {
        let buf = b"Content-Length: 0\r\n\r\n";
        let (n, body) = try_parse_frame(buf).unwrap().unwrap();
        assert_eq!(n, buf.len());
        assert!(body.is_empty());
    }

    #[test]
    fn parse_body_with_embedded_crlf_is_verbatim() {
        // The body is LEN bytes verbatim; CRLF inside isn't
        // a frame boundary. Regression: early drafts parsed
        // line-by-line and mis-handled JSON with embedded \r\n.
        let body = b"{\"msg\":\"hi\\r\\n\"}";
        let mut buf = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        buf.extend_from_slice(body);
        let (_, got) = try_parse_frame(&buf).unwrap().unwrap();
        assert_eq!(got, body);
    }

    // ── round-trip ──────────────────────────────────────────

    #[test]
    fn round_trip_body_survives_encode_parse() {
        let original = br#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}"#;
        let wire = encode_frame(original);
        let (_, parsed) = try_parse_frame(&wire).unwrap().unwrap();
        assert_eq!(parsed, original);
    }
}

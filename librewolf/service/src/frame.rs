//! The native-messaging wire: JSON preceded by a 4-byte length in native byte
//! order, written to stdout and read from stdin (MDN, *Native messaging*).
//!
//! Both caps here are ours, not the browser's:
//!
//! * What we **send** is capped at 1 MiB, because that is the largest message
//!   Firefox delivers *from* a native application *to* an extension; a longer
//!   frame would be dropped on the extension's side and look, from the tool's
//!   side, like a hang.
//! * What we **accept** is capped at 64 MiB. The browser would relay up to
//!   4 GB from the extension, but a read is bounded by the tool's `max`, and a
//!   much lower ceiling means a confused or hostile peer on the other end of
//!   stdin cannot make this process allocate without bound.
//!
//! A frame that breaks either rule is an error, never a partial read: the
//! stream would be off by a length prefix and every later message would be
//! misread.

use std::io::{self, ErrorKind, Read, Write};

use serde_json::Value;

/// The most Firefox will deliver from a native application to an extension.
pub const MAX_TO_EXTENSION: usize = 1 << 20;

/// Our own ceiling on what we will read from the extension.
pub const MAX_FROM_EXTENSION: usize = 64 << 20;

/// Read one length-prefixed JSON message. `Ok(None)` is a clean end of stream
/// before any byte of a header.
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Option<Value>> {
    let mut header = [0u8; 4];
    if !read_exactly(r, &mut header)? {
        return Ok(None);
    }
    let len = u32::from_ne_bytes(header) as usize;
    if len > MAX_FROM_EXTENSION {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "native message of {len} bytes exceeds the {MAX_FROM_EXTENSION}-byte cap this host accepts"
            ),
        ));
    }
    let mut body = vec![0u8; len];
    if !read_exactly(r, &mut body)? {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            format!("native message stopped after {len} bytes were promised"),
        ));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("native message of {len} bytes is not JSON: {e}"),
        )
    })?;
    Ok(Some(value))
}

/// Write one length-prefixed JSON message. Nothing is written when the message
/// would exceed the browser's cap: a truncated frame would desynchronize the
/// stream, so the refusal has to come before the first byte.
pub fn write_message<W: Write>(w: &mut W, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("cannot serialize: {e}")))?;
    if body.len() > MAX_TO_EXTENSION {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "refusing to send {} bytes to the extension: the browser delivers at most {MAX_TO_EXTENSION}",
                body.len()
            ),
        ));
    }
    w.write_all(&(body.len() as u32).to_ne_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Fill `buf` completely. `Ok(false)` only when zero bytes were read, i.e. a
/// clean end of stream; a short read is an error.
fn read_exactly<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return if filled == 0 {
                    Ok(false)
                } else {
                    Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        format!("stream ended after {filled} of {} bytes", buf.len()),
                    ))
                };
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(bytes.len() as u32).to_ne_bytes());
        out.extend_from_slice(bytes);
        out
    }

    #[test]
    fn a_message_survives_a_round_trip() {
        let msg = json!({"v": 1, "type": "command", "id": 7, "op": "read"});
        let mut wire = Vec::new();
        write_message(&mut wire, &msg).unwrap();
        let body = serde_json::to_vec(&msg).unwrap();
        assert_eq!(&wire[..4], &(body.len() as u32).to_ne_bytes());
        assert_eq!(&wire[4..], &body[..]);
        let mut cursor = std::io::Cursor::new(wire);
        assert_eq!(read_message(&mut cursor).unwrap(), Some(msg));
    }

    #[test]
    fn two_messages_in_a_row_are_two_messages() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({"a": 1})).unwrap();
        write_message(&mut wire, &json!({"b": 2})).unwrap();
        let mut cursor = std::io::Cursor::new(wire);
        assert_eq!(read_message(&mut cursor).unwrap(), Some(json!({"a": 1})));
        assert_eq!(read_message(&mut cursor).unwrap(), Some(json!({"b": 2})));
        assert_eq!(read_message(&mut cursor).unwrap(), None);
    }

    #[test]
    fn an_empty_stream_is_a_clean_end() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert_eq!(read_message(&mut cursor).unwrap(), None);
    }

    #[test]
    fn a_truncated_header_is_an_error_not_an_end() {
        let mut cursor = std::io::Cursor::new(vec![1u8, 0]);
        let err = read_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_truncated_body_is_an_error() {
        let mut wire = frame(b"{\"type\":\"hello\"}");
        wire.truncate(wire.len() - 2);
        let err = read_message(&mut std::io::Cursor::new(wire)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_body_that_is_not_json_is_refused() {
        let err = read_message(&mut std::io::Cursor::new(frame(b"not json"))).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn an_oversized_message_from_the_extension_is_refused_before_it_is_allocated() {
        // A header that promises more than the cap, with no body behind it: the
        // refusal must come from the header, or this test would try to allocate it.
        let mut header = Vec::new();
        header.extend_from_slice(&((MAX_FROM_EXTENSION as u32) + 1).to_ne_bytes());
        let err = read_message(&mut std::io::Cursor::new(header)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn an_oversized_message_to_the_extension_is_refused_without_writing_a_byte() {
        let big = json!({"text": "x".repeat(MAX_TO_EXTENSION + 1)});
        let mut wire = Vec::new();
        let err = write_message(&mut wire, &big).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert!(
            wire.is_empty(),
            "a refused message must leave the stream untouched"
        );
    }
}

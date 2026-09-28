//! Decoder for the AWS event stream wire format
//! (`application/vnd.amazon.eventstream`), the binary framing Amazon Bedrock's
//! ConverseStream uses for its response body.
//!
//! Every message is one frame:
//!
//! ```text
//! +-------------------+-------------------+-------------------+
//! | total length  u32 | headers length u32| prelude CRC   u32 |   prelude (12 bytes)
//! +-------------------+-------------------+-------------------+
//! | headers (headers length bytes)                            |
//! | payload (total - headers - 16 bytes)                      |
//! +-----------------------------------------------------------+
//! | message CRC u32 (over every byte before it)               |
//! +-----------------------------------------------------------+
//! ```
//!
//! All integers are big-endian; both CRCs are CRC-32 (IEEE 802.3 / zlib, the
//! ISO-HDLC parameters). A header is a one-byte name length, the UTF-8 name,
//! a one-byte value type and a value whose size depends on the type.
//!
//! Network chunks can split a frame anywhere — inside the prelude, a header or
//! a multi-byte UTF-8 character in the payload — so [`FrameDecoder`] buffers raw
//! bytes and yields only complete, checksum-verified frames. A checksum
//! mismatch or a malformed frame is an error, never skipped: once framing is
//! lost there is no way to find the next frame boundary.
//!
//! The CRC is a 256-entry table built at compile time rather than a
//! dependency: no CRC crate is in this crate's dependency tree, and the
//! algorithm is a dozen lines checked against published test vectors.

use std::fmt;

/// Smallest possible frame: 12-byte prelude + 4-byte message CRC.
const MIN_FRAME_LEN: usize = 16;
/// Largest frame accepted. The AWS event stream implementations cap a message
/// at 16 MiB (`aws-c-event-stream`'s `AWS_EVENT_STREAM_MAX_MESSAGE_SIZE`);
/// anything larger is corrupt framing, not a message to buffer.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;
/// Largest headers section accepted (`AWS_EVENT_STREAM_MAX_HEADERS_SIZE`).
const MAX_HEADERS_LEN: usize = 128 * 1024;

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC-32 (reflected polynomial `0xEDB88320`, init and xor-out `0xFFFFFFFF`)
/// — the checksum the event stream prelude and message trailer carry.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC32_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// A decoded header value, one variant per wire type (0–9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderValue {
    /// Types 0 (true) and 1 (false); no value bytes on the wire.
    Bool(bool),
    /// Type 2: one signed byte.
    Byte(i8),
    /// Type 3.
    Int16(i16),
    /// Type 4.
    Int32(i32),
    /// Type 5.
    Int64(i64),
    /// Type 6: u16 length + bytes.
    Bytes(Vec<u8>),
    /// Type 7: u16 length + UTF-8 bytes.
    String(String),
    /// Type 8: milliseconds since the epoch, i64.
    Timestamp(i64),
    /// Type 9: 16 bytes.
    Uuid([u8; 16]),
}

/// One complete, checksum-verified frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Frame {
    /// The value of a string-typed header, if present.
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(n, v)| match v {
            HeaderValue::String(s) if n == name => Some(s.as_str()),
            _ => None,
        })
    }
}

/// Why a byte stream could not be decoded into frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The prelude CRC does not match its first eight bytes.
    PreludeChecksum { expected: u32, actual: u32 },
    /// The trailing CRC does not match the frame.
    MessageChecksum { expected: u32, actual: u32 },
    /// Structurally invalid: impossible lengths, a header running past its
    /// section, an unknown value type, non-UTF-8 text.
    Malformed(String),
    /// The stream ended with this many bytes of an unfinished frame buffered.
    Truncated { buffered: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreludeChecksum { expected, actual } => write!(
                f,
                "event stream prelude checksum mismatch (frame says {expected:#010x}, computed {actual:#010x})"
            ),
            Self::MessageChecksum { expected, actual } => write!(
                f,
                "event stream message checksum mismatch (frame says {expected:#010x}, computed {actual:#010x})"
            ),
            Self::Malformed(why) => write!(f, "malformed event stream frame: {why}"),
            Self::Truncated { buffered } => write!(
                f,
                "event stream ended inside a frame ({buffered} bytes of an incomplete frame buffered)"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

/// Incremental frame decoder: feed it network chunks with [`push`](Self::push),
/// pull complete frames with [`next_frame`](Self::next_frame), and call
/// [`finish`](Self::finish) at end of stream to reject a trailing partial frame.
///
/// After an error the decoder is poisoned (every later call returns the same
/// error): framing is lost and nothing after it can be trusted.
#[derive(Debug, Default)]
pub(crate) struct FrameDecoder {
    buf: Vec<u8>,
    failed: Option<FrameError>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw bytes from the network.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, `Ok(None)` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        match self.try_decode() {
            Ok(Some((frame, used))) => {
                self.buf.drain(..used);
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                self.failed = Some(e.clone());
                Err(e)
            }
        }
    }

    /// End of stream: an error if an incomplete frame is still buffered.
    pub fn finish(&self) -> Result<(), FrameError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(FrameError::Truncated {
                buffered: self.buf.len(),
            })
        }
    }

    fn try_decode(&self) -> Result<Option<(Frame, usize)>, FrameError> {
        let buf = &self.buf;
        if buf.len() < 12 {
            return Ok(None);
        }
        // Check the prelude before trusting its lengths: a corrupt length
        // would otherwise make us wait for bytes that never come.
        let expected = be_u32(&buf[8..12]);
        let actual = crc32(&buf[..8]);
        if expected != actual {
            return Err(FrameError::PreludeChecksum { expected, actual });
        }
        let total = be_u32(&buf[0..4]) as usize;
        let headers_len = be_u32(&buf[4..8]) as usize;
        if total < MIN_FRAME_LEN {
            return Err(FrameError::Malformed(format!(
                "total length {total} is below the 16-byte minimum"
            )));
        }
        if total > MAX_FRAME_LEN {
            return Err(FrameError::Malformed(format!(
                "total length {total} exceeds the {MAX_FRAME_LEN}-byte maximum"
            )));
        }
        if headers_len > MAX_HEADERS_LEN || headers_len > total - MIN_FRAME_LEN {
            return Err(FrameError::Malformed(format!(
                "headers length {headers_len} does not fit a {total}-byte frame"
            )));
        }
        if buf.len() < total {
            return Ok(None);
        }
        let expected = be_u32(&buf[total - 4..total]);
        let actual = crc32(&buf[..total - 4]);
        if expected != actual {
            return Err(FrameError::MessageChecksum { expected, actual });
        }
        let headers = parse_headers(&buf[12..12 + headers_len])?;
        let payload = buf[12 + headers_len..total - 4].to_vec();
        Ok(Some((Frame { headers, payload }, total)))
    }
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn parse_headers(mut b: &[u8]) -> Result<Vec<(String, HeaderValue)>, FrameError> {
    fn take<'a>(b: &mut &'a [u8], n: usize, what: &str) -> Result<&'a [u8], FrameError> {
        if b.len() < n {
            return Err(FrameError::Malformed(format!(
                "header {what} needs {n} bytes, {} left",
                b.len()
            )));
        }
        let (head, tail) = b.split_at(n);
        *b = tail;
        Ok(head)
    }
    fn u16_len(b: &mut &[u8], what: &str) -> Result<usize, FrameError> {
        let l = take(b, 2, what)?;
        Ok(u16::from_be_bytes([l[0], l[1]]) as usize)
    }

    let mut headers = Vec::new();
    while !b.is_empty() {
        let name_len = take(&mut b, 1, "name length")?[0] as usize;
        if name_len == 0 {
            return Err(FrameError::Malformed("empty header name".into()));
        }
        let name = std::str::from_utf8(take(&mut b, name_len, "name")?)
            .map_err(|_| FrameError::Malformed("header name is not UTF-8".into()))?
            .to_string();
        let ty = take(&mut b, 1, "value type")?[0];
        let value = match ty {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Byte(take(&mut b, 1, "byte value")?[0] as i8),
            3 => {
                let v = take(&mut b, 2, "int16 value")?;
                HeaderValue::Int16(i16::from_be_bytes([v[0], v[1]]))
            }
            4 => {
                let v = take(&mut b, 4, "int32 value")?;
                HeaderValue::Int32(i32::from_be_bytes([v[0], v[1], v[2], v[3]]))
            }
            5 | 8 => {
                let v = take(&mut b, 8, "int64 value")?;
                let mut a = [0u8; 8];
                a.copy_from_slice(v);
                let n = i64::from_be_bytes(a);
                if ty == 5 {
                    HeaderValue::Int64(n)
                } else {
                    HeaderValue::Timestamp(n)
                }
            }
            6 => {
                let n = u16_len(&mut b, "bytes length")?;
                HeaderValue::Bytes(take(&mut b, n, "bytes value")?.to_vec())
            }
            7 => {
                let n = u16_len(&mut b, "string length")?;
                let s = std::str::from_utf8(take(&mut b, n, "string value")?).map_err(|_| {
                    FrameError::Malformed(format!("header `{name}` string is not UTF-8"))
                })?;
                HeaderValue::String(s.to_string())
            }
            9 => {
                let v = take(&mut b, 16, "uuid value")?;
                let mut a = [0u8; 16];
                a.copy_from_slice(v);
                HeaderValue::Uuid(a)
            }
            other => {
                return Err(FrameError::Malformed(format!(
                    "header `{name}` has unknown value type {other}"
                )))
            }
        };
        headers.push((name, value));
    }
    Ok(headers)
}

/// Test-only encoder, the inverse of [`FrameDecoder`], shared with the
/// Bedrock provider's unit tests.
#[cfg(test)]
pub(crate) fn encode_frame(headers: &[(&str, HeaderValue)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (name, value) in headers {
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        match value {
            HeaderValue::Bool(true) => h.push(0),
            HeaderValue::Bool(false) => h.push(1),
            HeaderValue::Byte(v) => {
                h.push(2);
                h.push(*v as u8);
            }
            HeaderValue::Int16(v) => {
                h.push(3);
                h.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::Int32(v) => {
                h.push(4);
                h.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::Int64(v) => {
                h.push(5);
                h.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::Bytes(v) => {
                h.push(6);
                h.extend_from_slice(&(v.len() as u16).to_be_bytes());
                h.extend_from_slice(v);
            }
            HeaderValue::String(v) => {
                h.push(7);
                h.extend_from_slice(&(v.len() as u16).to_be_bytes());
                h.extend_from_slice(v.as_bytes());
            }
            HeaderValue::Timestamp(v) => {
                h.push(8);
                h.extend_from_slice(&v.to_be_bytes());
            }
            HeaderValue::Uuid(v) => {
                h.push(9);
                h.extend_from_slice(v);
            }
        }
    }
    let total = (16 + h.len() + payload.len()) as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    let prelude_crc = crc32(&out);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    let msg_crc = crc32(&out);
    out.extend_from_slice(&msg_crc.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn decode_one(bytes: &[u8]) -> Result<Frame, FrameError> {
        let mut d = FrameDecoder::new();
        d.push(bytes);
        let frame = d.next_frame()?.expect("a complete frame");
        d.finish()?;
        Ok(frame)
    }

    #[test]
    fn crc32_check_value() {
        // The standard CRC-32/ISO-HDLC check value (CRC RevEng catalogue).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    // ------------------------------------------------------------------
    // Published test vectors: aws/aws-sdk-go (Apache-2.0),
    // private/protocol/eventstream/testdata/{encoded,decoded}/{positive,negative}/*
    // https://github.com/aws/aws-sdk-go/tree/main/private/protocol/eventstream/testdata
    // The same suite ships with the other AWS SDKs' eventstream codecs. The
    // `encoded` files are reproduced here as hex; the expected values come
    // from the matching `decoded` JSON files (and, for negatives, their
    // one-line expected error).
    // ------------------------------------------------------------------

    const PAYLOAD: &[u8] = b"{'foo':'bar'}";

    #[test]
    fn vector_positive_empty_message() {
        // decoded: total_length 16, headers_length 0, prelude_crc 96618731,
        // no headers, empty payload, message_crc 2107164927.
        let bytes = hex("000000100000000005c248eb7d98c8ff");
        assert_eq!(be_u32(&bytes[8..12]), 96_618_731);
        assert_eq!(be_u32(&bytes[12..16]), 2_107_164_927);
        let f = decode_one(&bytes).unwrap();
        assert!(f.headers.is_empty());
        assert!(f.payload.is_empty());
    }

    #[test]
    fn vector_positive_payload_no_headers() {
        let bytes = hex("0000001d00000000fd528c5a7b27666f6f273a27626172277dc3653936");
        let f = decode_one(&bytes).unwrap();
        assert!(f.headers.is_empty());
        assert_eq!(f.payload, PAYLOAD);
    }

    #[test]
    fn vector_positive_payload_one_str_header() {
        let bytes = hex(
            "0000003d0000002007fd83960c636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b27666f6f273a27626172277d8d9c08b1",
        );
        let f = decode_one(&bytes).unwrap();
        assert_eq!(f.header_str("content-type"), Some("application/json"));
        assert_eq!(f.payload, PAYLOAD);
    }

    #[test]
    fn vector_positive_int32_header() {
        let bytes = hex(
            "0000002d0000001041c424b80a6576656e742d74797065040000a00c7b27666f6f273a27626172277d36f480a0",
        );
        let f = decode_one(&bytes).unwrap();
        assert_eq!(
            f.headers,
            vec![("event-type".to_string(), HeaderValue::Int32(40972))]
        );
        assert_eq!(f.payload, PAYLOAD);
    }

    const ALL_HEADERS: &str = "000000cc000000af0fae64ca0a6576656e742d74797065040000a00c0c636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e0a626f6f6c2066616c73650109626f6f6c207472756500046279746502cf08627974652062756606001449276d2061206c6974746c6520746561706f74210974696d657374616d70080000000000845fed05696e74313603002a05696e7436340500000000028757b20475756964090102030405060708090a0b0c0d0e0f107b27666f6f273a27626172277daba5f10c";

    #[test]
    fn vector_positive_all_headers() {
        let bytes = hex(ALL_HEADERS);
        let f = decode_one(&bytes).unwrap();
        let expected = vec![
            ("event-type".to_string(), HeaderValue::Int32(40972)),
            (
                "content-type".to_string(),
                HeaderValue::String("application/json".into()),
            ),
            ("bool false".to_string(), HeaderValue::Bool(false)),
            ("bool true".to_string(), HeaderValue::Bool(true)),
            ("byte".to_string(), HeaderValue::Byte(-49)),
            (
                "byte buf".to_string(),
                HeaderValue::Bytes(b"I'm a little teapot!".to_vec()),
            ),
            ("timestamp".to_string(), HeaderValue::Timestamp(8_675_309)),
            ("int16".to_string(), HeaderValue::Int16(42)),
            ("int64".to_string(), HeaderValue::Int64(42_424_242)),
            (
                "uuid".to_string(),
                HeaderValue::Uuid([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
            ),
        ];
        assert_eq!(f.headers, expected);
        assert_eq!(f.payload, PAYLOAD);
        // The encoder is the decoder's inverse on this vector, byte for byte.
        let refs: Vec<(&str, HeaderValue)> = expected
            .iter()
            .map(|(n, v)| (n.as_str(), v.clone()))
            .collect();
        assert_eq!(encode_frame(&refs, PAYLOAD), bytes);
    }

    #[test]
    fn vector_negative_corrupted_header_len() {
        // expected: "Prelude checksum mismatch"
        let bytes = hex(
            "0000003d0000002107fd83960c636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b27666f6f273a27626172277d8d9c08b1",
        );
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert!(matches!(
            d.next_frame(),
            Err(FrameError::PreludeChecksum { .. })
        ));
    }

    #[test]
    fn vector_negative_corrupted_length() {
        // expected: "Prelude checksum mismatch". The corrupt length (62)
        // exceeds the 61 bytes present: the prelude check must fire before
        // the decoder waits for bytes that will never come.
        let bytes = hex(
            "0000003e0000002007fd83960c636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b27666f6f273a27626172277d8d9c08b1",
        );
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert!(matches!(
            d.next_frame(),
            Err(FrameError::PreludeChecksum { .. })
        ));
    }

    #[test]
    fn vector_negative_corrupted_headers() {
        // expected: "Message checksum mismatch"
        let bytes = hex(
            "0000003d0000002007fd83960c636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b61666f6f273a27626172277d8d9c08b1",
        );
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert!(matches!(
            d.next_frame(),
            Err(FrameError::MessageChecksum { .. })
        ));
    }

    #[test]
    fn vector_negative_corrupted_payload() {
        // expected: "Message checksum mismatch"
        let bytes = hex("0000001d00000000fd528c5a5b27666f6f273a27626172277dc3653936");
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert!(matches!(
            d.next_frame(),
            Err(FrameError::MessageChecksum { .. })
        ));
        // Poisoned: the error sticks.
        assert!(d.next_frame().is_err());
        assert!(d.finish().is_err());
    }

    /// Three frames back to back, fed in every chunk size from 1 byte up —
    /// every split point lands somewhere, including inside the prelude, inside
    /// a header, and between the bytes of a multi-byte UTF-8 character.
    #[test]
    fn frames_survive_arbitrary_chunking() {
        let payload = "héllo, 世界 🌍".as_bytes();
        let mut stream = Vec::new();
        stream.extend(encode_frame(
            &[(":event-type", HeaderValue::String("a".into()))],
            payload,
        ));
        stream.extend(hex(ALL_HEADERS));
        stream.extend(encode_frame(&[], b""));

        for chunk in 1..=stream.len() {
            let mut d = FrameDecoder::new();
            let mut frames = Vec::new();
            for piece in stream.chunks(chunk) {
                d.push(piece);
                while let Some(f) = d.next_frame().unwrap() {
                    frames.push(f);
                }
            }
            d.finish().unwrap();
            assert_eq!(frames.len(), 3, "chunk size {chunk}");
            assert_eq!(frames[0].payload, payload);
            assert_eq!(frames[0].header_str(":event-type"), Some("a"));
            assert_eq!(frames[1].payload, PAYLOAD);
            assert!(frames[2].payload.is_empty());
        }
    }

    #[test]
    fn truncated_frame_at_end_of_stream_is_an_error() {
        let frame = encode_frame(&[], b"payload");
        for cut in 1..frame.len() {
            let mut d = FrameDecoder::new();
            d.push(&frame[..cut]);
            assert_eq!(d.next_frame(), Ok(None), "cut at {cut}");
            assert_eq!(d.finish(), Err(FrameError::Truncated { buffered: cut }));
        }
    }

    #[test]
    fn headers_running_past_their_section_are_malformed() {
        // A string header claiming 0xFFFF bytes inside a small headers section,
        // with valid CRCs — only the header walk can catch it.
        let mut h = vec![1u8, b'x', 7, 0xFF, 0xFF];
        h.extend_from_slice(b"abc");
        let total = (16 + h.len()) as u32;
        let mut f = Vec::new();
        f.extend_from_slice(&total.to_be_bytes());
        f.extend_from_slice(&(h.len() as u32).to_be_bytes());
        let c = crc32(&f);
        f.extend_from_slice(&c.to_be_bytes());
        f.extend_from_slice(&h);
        let c = crc32(&f);
        f.extend_from_slice(&c.to_be_bytes());
        let mut d = FrameDecoder::new();
        d.push(&f);
        assert!(matches!(d.next_frame(), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn unknown_header_type_is_malformed() {
        let h = vec![1u8, b'x', 10];
        let total = (16 + h.len()) as u32;
        let mut f = Vec::new();
        f.extend_from_slice(&total.to_be_bytes());
        f.extend_from_slice(&(h.len() as u32).to_be_bytes());
        let c = crc32(&f);
        f.extend_from_slice(&c.to_be_bytes());
        f.extend_from_slice(&h);
        let c = crc32(&f);
        f.extend_from_slice(&c.to_be_bytes());
        let mut d = FrameDecoder::new();
        d.push(&f);
        assert!(matches!(d.next_frame(), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn oversized_length_is_rejected_not_buffered() {
        let total = (MAX_FRAME_LEN as u32) + 1;
        let mut f = Vec::new();
        f.extend_from_slice(&total.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        let c = crc32(&f);
        f.extend_from_slice(&c.to_be_bytes());
        let mut d = FrameDecoder::new();
        d.push(&f);
        assert!(matches!(d.next_frame(), Err(FrameError::Malformed(_))));
    }
}

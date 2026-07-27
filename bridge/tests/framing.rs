use std::io::{self, Cursor, Read};

use tool4d_lsp_stdio::relay::normalize_lsp_stream;

fn tool4d_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = format!("Content-Length:{}\r\n\r\n", body.len()).into_bytes();

    frame.extend_from_slice(body);
    frame
}

fn canonical_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();

    frame.extend_from_slice(body);
    frame
}

fn normalize(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();

    normalize_lsp_stream(&mut Cursor::new(input), &mut output)?;

    Ok(output)
}

/// A reader that returns only a small number of bytes per read.
///
/// TCP does not preserve message boundaries. This verifies that framing does
/// not depend on receiving a complete header or body in one read.
struct ChunkedReader<R> {
    inner: R,
    chunk_size: usize,
}

impl<R> ChunkedReader<R> {
    fn new(inner: R, chunk_size: usize) -> Self {
        Self { inner, chunk_size }
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = buffer.len().min(self.chunk_size);
        self.inner.read(&mut buffer[..limit])
    }
}

#[test]
fn normalizes_tool4d_content_length_header() {
    let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
    let input = tool4d_frame(body);

    let output = normalize(&input).expect("normalization should succeed");

    assert_eq!(output, canonical_frame(body));
}

#[test]
fn accepts_an_already_canonical_header() {
    let body = br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
    let input = canonical_frame(body);

    let output = normalize(&input).expect("normalization should succeed");

    assert_eq!(output, input);
}

#[test]
fn accepts_case_insensitive_content_length_name() {
    let body = br#"{"jsonrpc":"2.0","id":2,"result":{}}"#;

    let mut input = format!("content-length:{}\r\n\r\n", body.len()).into_bytes();

    input.extend_from_slice(body);

    let output = normalize(&input).expect("normalization should succeed");

    assert_eq!(output, canonical_frame(body));
}

#[test]
fn handles_single_byte_reads() {
    let body = br#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
    let input = tool4d_frame(body);

    let reader = ChunkedReader::new(Cursor::new(input), 1);

    let mut output = Vec::new();

    normalize_lsp_stream(reader, &mut output)
        .expect("normalization should handle fragmented input");

    assert_eq!(output, canonical_frame(body));
}

#[test]
fn handles_multiple_messages() {
    let first_body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;

    let second_body =
        br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"test"}}"#;

    let mut input = tool4d_frame(first_body);
    input.extend_from_slice(&canonical_frame(second_body));

    let mut expected = canonical_frame(first_body);
    expected.extend_from_slice(&canonical_frame(second_body));

    let output = normalize(&input).expect("normalization should succeed");

    assert_eq!(output, expected);
}

#[test]
fn preserves_body_bytes_exactly() {
    let body = [0x00, 0x01, 0x02, 0x7f, 0x80, 0xfe, 0xff];

    let input = tool4d_frame(&body);

    let output = normalize(&input).expect("normalization should succeed");

    assert_eq!(output, canonical_frame(&body));
}

#[test]
fn preserves_content_type_header() {
    let body = br#"{"jsonrpc":"2.0","id":4,"result":null}"#;

    let mut input = format!(
        "Content-Length:{}\r\n\
		 Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
		 \r\n",
        body.len()
    )
    .into_bytes();

    input.extend_from_slice(body);

    let mut expected = format!(
        "Content-Length: {}\r\n\
		 Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
		 \r\n",
        body.len()
    )
    .into_bytes();

    expected.extend_from_slice(body);

    let output = normalize(&input).expect("normalization should succeed");

    assert_eq!(output, expected);
}

#[test]
fn rejects_missing_content_length() {
    let input = b"Content-Type: application/json\r\n\r\n{}";

    let error = normalize(input).expect_err("missing Content-Length must fail");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn rejects_invalid_content_length() {
    let input = b"Content-Length: invalid\r\n\r\n{}";

    let error = normalize(input).expect_err("invalid Content-Length must fail");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn rejects_duplicate_content_length() {
    let input = b"Content-Length: 2\r\n\
				  Content-Length: 2\r\n\
				  \r\n\
				  {}";

    let error = normalize(input).expect_err("duplicate Content-Length must fail");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn rejects_truncated_body() {
    let input = b"Content-Length: 10\r\n\r\n{}";

    let error = normalize(input).expect_err("a truncated body must fail");

    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn rejects_lf_only_headers() {
    let input = b"Content-Length: 2\n\n{}";

    let error = normalize(input).expect_err("LSP requires CRLF header delimiters");

    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
        ),
        "unexpected error kind: {:?}",
        error.kind()
    );
}

#[test]
fn empty_stream_is_a_clean_end_of_stream() {
    let output = normalize(&[]).expect("empty input should be accepted");

    assert!(output.is_empty());
}

use std::{
    collections::HashSet,
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use serde_json::{Value, json};

const MAX_LSP_HEADER_SIZE: usize = 64 * 1024;
const MAX_LSP_BODY_SIZE: usize = 256 * 1024 * 1024;

/// Shared compatibility state for both directions of an LSP session.
///
/// Zed-to-tool4d requests are inspected so that responses can be associated
/// with the method that produced them. This is currently used only for
/// `textDocument/diagnostic`.
#[derive(Default)]
pub struct CompatibilityState {
    pending_diagnostic_requests: Mutex<HashSet<RequestId>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RequestId {
    Number(String),
    String(String),
}

impl RequestId {
    fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => Some(Self::Number(number.to_string())),

            Value::String(value) => Some(Self::String(value.clone())),

            _ => None,
        }
    }
}

/// An event reported by one side of the LSP relay.
#[derive(Debug)]
pub enum RelayEvent {
    /// The editor closed the adapter's standard input.
    StdinClosed,

    /// The TCP peer closed the connection.
    SocketClosed,

    /// One side of the relay failed.
    Error {
        direction: &'static str,
        error: io::Error,
    },
}

/// A running bidirectional relay.
pub struct Relay {
    control_stream: TcpStream,
    events: Receiver<RelayEvent>,
}

impl Relay {
    /// Starts the bidirectional LSP relay.
    ///
    /// The relay:
    ///
    /// - records IDs of `textDocument/diagnostic` requests;
    /// - normalizes LSP `Content-Length` headers;
    /// - repairs invalid `null` diagnostic responses from tool4d;
    /// - otherwise preserves message bodies byte-for-byte.
    pub fn start(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(false)?;
        stream.set_nodelay(true)?;

        let socket_reader = stream.try_clone()?;
        let socket_writer = stream.try_clone()?;
        let control_stream = stream;

        let compatibility_state = Arc::new(CompatibilityState::default());

        let (event_sender, event_receiver) = mpsc::channel();

        start_stdin_to_socket(
            socket_writer,
            event_sender.clone(),
            Arc::clone(&compatibility_state),
        );

        start_socket_to_stdout(socket_reader, event_sender, compatibility_state);

        Ok(Self {
            control_stream,
            events: event_receiver,
        })
    }

    /// Returns the channel used by relay workers to notify the supervisor.
    pub fn events(&self) -> &Receiver<RelayEvent> {
        &self.events
    }

    /// Interrupts both directions of the TCP connection.
    pub fn shutdown(&self) {
        match self.control_stream.shutdown(Shutdown::Both) {
            Ok(()) => {}

            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
                ) => {}

            Err(error) => {
                eprintln!(
                    "tool4d-lsp-stdio: failed to close the LSP socket: \
                     {error}"
                );
            }
        }
    }
}

fn start_stdin_to_socket(
    mut socket_writer: TcpStream,
    event_sender: Sender<RelayEvent>,
    compatibility_state: Arc<CompatibilityState>,
) {
    thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();

            relay_editor_stream(&mut stdin, &mut socket_writer, &compatibility_state)?;

            match socket_writer.shutdown(Shutdown::Write) {
                Ok(()) => {}

                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
                    ) => {}

                Err(error) => return Err(error),
            }

            Ok(())
        })();

        let event = match result {
            Ok(()) => RelayEvent::StdinClosed,

            Err(error) => RelayEvent::Error {
                direction: "stdin-to-socket",
                error,
            },
        };

        let _ = event_sender.send(event);
    });
}

fn start_socket_to_stdout(
    mut socket_reader: TcpStream,
    event_sender: Sender<RelayEvent>,
    compatibility_state: Arc<CompatibilityState>,
) {
    thread::spawn(move || {
        let result = {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
        
            relay_tool4d_stream(&mut socket_reader, &mut stdout, &compatibility_state)
        };

        let event = match result {
            Ok(()) => RelayEvent::SocketClosed,

            Err(error) => RelayEvent::Error {
                direction: "socket-to-stdout",
                error,
            },
        };

        let _ = event_sender.send(event);
    });
}

/// Relays editor-to-tool4d LSP messages.
///
/// Message bodies are forwarded byte-for-byte. Requests whose method is
/// `textDocument/diagnostic` are recorded before they are sent to tool4d.
pub fn relay_editor_stream<R, W>(
    mut input: R,
    mut output: W,
    state: &CompatibilityState,
) -> io::Result<()>
where
    R: Read,
    W: Write,
{
    while let Some(frame) = read_lsp_frame(&mut input)? {
        remember_diagnostic_request(&frame.body, state)?;
        write_lsp_frame(&mut output, &frame)?;
    }

    output.flush()
}

/// Relays tool4d-to-editor LSP messages.
///
/// If a response corresponds to a recorded `textDocument/diagnostic`
/// request and has `result: null`, it is converted to an empty full
/// diagnostic report.
pub fn relay_tool4d_stream<R, W>(
    mut input: R,
    mut output: W,
    state: &CompatibilityState,
) -> io::Result<()>
where
    R: Read,
    W: Write,
{
    while let Some(mut frame) = read_lsp_frame(&mut input)? {
        rewrite_null_diagnostic_response(&mut frame.body, state)?;
        write_lsp_frame(&mut output, &frame)?;
    }

    output.flush()
}

/// Reads LSP frames and writes them with canonical `Content-Length` headers.
///
/// This function does not inspect or modify JSON-RPC bodies.
pub fn normalize_lsp_stream<R, W>(mut input: R, mut output: W) -> io::Result<()>
where
    R: Read,
    W: Write,
{
    while let Some(frame) = read_lsp_frame(&mut input)? {
        write_lsp_frame(&mut output, &frame)?;
    }

    output.flush()
}

struct LspFrame {
    additional_headers: Vec<Vec<u8>>,
    body: Vec<u8>,
}

fn read_lsp_frame<R>(input: &mut R) -> io::Result<Option<LspFrame>>
where
    R: Read,
{
    let Some(header) = read_lsp_header(input)? else {
        return Ok(None);
    };

    let parsed_header = parse_lsp_header(&header)?;

    if parsed_header.content_length > MAX_LSP_BODY_SIZE {
        return Err(invalid_data(format!(
            "LSP body length {} exceeds the maximum of {} bytes",
            parsed_header.content_length, MAX_LSP_BODY_SIZE
        )));
    }

    let mut body = vec![0_u8; parsed_header.content_length];

    input.read_exact(&mut body).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "LSP body was truncated: expected {} bytes",
                    parsed_header.content_length
                ),
            )
        } else {
            error
        }
    })?;

    Ok(Some(LspFrame {
        additional_headers: parsed_header.additional_headers,
        body,
    }))
}

fn write_lsp_frame<W>(output: &mut W, frame: &LspFrame) -> io::Result<()>
where
    W: Write,
{
    write!(output, "Content-Length: {}\r\n", frame.body.len())?;

    for header in &frame.additional_headers {
        output.write_all(header)?;
        output.write_all(b"\r\n")?;
    }

    output.write_all(b"\r\n")?;
    output.write_all(&frame.body)?;
    output.flush()
}

fn remember_diagnostic_request(body: &[u8], state: &CompatibilityState) -> io::Result<()> {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        /*
         * Framing remains transparent for non-JSON or malformed payloads.
         * tool4d will decide how to handle the payload.
         */
        return Ok(());
    };

    if message.get("method").and_then(Value::as_str) != Some("textDocument/diagnostic") {
        return Ok(());
    }

    let Some(request_id) = message.get("id").and_then(RequestId::from_json) else {
        /*
         * A JSON-RPC notification has no response and therefore must not be
         * added to the pending request set.
         */
        return Ok(());
    };

    lock_pending_diagnostics(state)?.insert(request_id);

    Ok(())
}

fn rewrite_null_diagnostic_response(
    body: &mut Vec<u8>,
    state: &CompatibilityState,
) -> io::Result<()> {
    let Ok(mut message) = serde_json::from_slice::<Value>(body.as_slice()) else {
        return Ok(());
    };

    let Some(request_id) = message.get("id").and_then(RequestId::from_json) else {
        return Ok(());
    };

    /*
     * Consume the pending ID as soon as any matching response arrives.
     * This includes valid reports and JSON-RPC error responses.
     */
    let is_diagnostic_response = lock_pending_diagnostics(state)?.remove(&request_id);

    if !is_diagnostic_response {
        return Ok(());
    }

    if message.get("error").is_some() {
        return Ok(());
    }

    if message.get("result") != Some(&Value::Null) {
        return Ok(());
    }

    let Some(object) = message.as_object_mut() else {
        return Ok(());
    };

    object.insert(
        "result".to_owned(),
        json!({
            "kind": "full",
            "items": []
        }),
    );

    *body = serde_json::to_vec(&message).map_err(|error| {
        invalid_data(format!(
            "failed to serialize repaired diagnostic response: {error}"
        ))
    })?;

    Ok(())
}

fn lock_pending_diagnostics(
    state: &CompatibilityState,
) -> io::Result<MutexGuard<'_, HashSet<RequestId>>> {
    state
        .pending_diagnostic_requests
        .lock()
        .map_err(|_| io::Error::other("pending diagnostic request state is poisoned"))
}

struct ParsedHeader {
    content_length: usize,
    additional_headers: Vec<Vec<u8>>,
}

/// Reads one LSP header block, excluding the final CRLF-CRLF delimiter.
///
/// `Ok(None)` means the stream reached EOF before another frame started.
fn read_lsp_header<R>(input: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: Read,
{
    const HEADER_END: &[u8] = b"\r\n\r\n";

    let mut header = Vec::new();
    let mut byte = [0_u8; 1];

    loop {
        match input.read(&mut byte) {
            Ok(0) if header.is_empty() => return Ok(None),

            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "LSP stream ended in the middle of a header",
                ));
            }

            Ok(_) => {
                header.push(byte[0]);

                if header.len() > MAX_LSP_HEADER_SIZE {
                    return Err(invalid_data(format!(
                        "LSP header exceeds the maximum of \
                         {MAX_LSP_HEADER_SIZE} bytes"
                    )));
                }

                if header.ends_with(HEADER_END) {
                    header.truncate(header.len() - HEADER_END.len());

                    return Ok(Some(header));
                }
            }

            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                continue;
            }

            Err(error) => return Err(error),
        }
    }
}

fn parse_lsp_header(header: &[u8]) -> io::Result<ParsedHeader> {
    if header.is_empty() {
        return Err(invalid_data("empty LSP header"));
    }

    let mut content_length = None;
    let mut additional_headers = Vec::new();
    let mut remaining = header;

    loop {
        let (line, rest) = match find_crlf(remaining) {
            Some(position) => (&remaining[..position], Some(&remaining[position + 2..])),

            None => (remaining, None),
        };

        if line.is_empty() {
            return Err(invalid_data("unexpected empty line inside LSP header"));
        }

        parse_lsp_header_line(line, &mut content_length, &mut additional_headers)?;

        match rest {
            Some(rest) => {
                remaining = rest;

                if remaining.is_empty() {
                    return Err(invalid_data("unexpected empty line inside LSP header"));
                }
            }

            None => break,
        }
    }

    let content_length =
        content_length.ok_or_else(|| invalid_data("missing Content-Length header"))?;

    Ok(ParsedHeader {
        content_length,
        additional_headers,
    })
}

fn find_crlf(value: &[u8]) -> Option<usize> {
    value.windows(2).position(|window| window == b"\r\n")
}

fn parse_lsp_header_line(
    line: &[u8],
    content_length: &mut Option<usize>,
    additional_headers: &mut Vec<Vec<u8>>,
) -> io::Result<()> {
    if line.contains(&b'\r') || line.contains(&b'\n') {
        return Err(invalid_data("LSP header lines must be separated by CRLF"));
    }

    let colon_position = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| invalid_data("LSP header line has no colon"))?;

    let name = trim_ascii_whitespace(&line[..colon_position]);

    let value = trim_ascii_whitespace(&line[colon_position + 1..]);

    if name.is_empty() {
        return Err(invalid_data("LSP header name is empty"));
    }

    if name.eq_ignore_ascii_case(b"Content-Length") {
        if content_length.is_some() {
            return Err(invalid_data("duplicate Content-Length header"));
        }

        let value = std::str::from_utf8(value)
            .map_err(|_| invalid_data("Content-Length contains non-UTF-8 bytes"))?;

        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid_data("Content-Length is not a decimal integer"));
        }

        let length = value
            .parse::<usize>()
            .map_err(|_| invalid_data("Content-Length is out of range"))?;

        *content_length = Some(length);
    } else {
        additional_headers.push(line.to_vec());
    }

    Ok(())
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[1..];
    }

    while value.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[..value.len() - 1];
    }

    value
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Relays arbitrary input and output streams to a TCP connection.
///
/// This generic helper retains raw byte-relay behavior. Production tool4d
/// sessions use `Relay`, which applies LSP framing and compatibility handling.
pub fn streams_to_tcp<R, W>(mut input: R, mut output: W, stream: TcpStream) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write,
{
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;

    let mut socket_reader = stream.try_clone()?;
    let mut socket_writer = stream;

    thread::spawn(move || {
        let _ = io::copy(&mut input, &mut socket_writer);
        let _ = socket_writer.shutdown(Shutdown::Write);
    });

    io::copy(&mut socket_reader, &mut output)?;
    output.flush()
}

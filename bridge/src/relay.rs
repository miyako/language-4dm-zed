use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

const MAX_LSP_HEADER_SIZE: usize = 64 * 1024;
const MAX_LSP_BODY_SIZE: usize = 256 * 1024 * 1024;

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
    /// Starts relaying:
    ///
    /// - editor stdin to tool4d without modification;
    /// - tool4d TCP output to editor stdout with normalized LSP headers.
    pub fn start(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(false)?;
        stream.set_nodelay(true)?;

        let socket_reader = stream.try_clone()?;
        let socket_writer = stream.try_clone()?;
        let control_stream = stream;

        let (event_sender, event_receiver) = mpsc::channel();

        start_stdin_to_socket(socket_writer, event_sender.clone());
        start_socket_to_stdout(socket_reader, event_sender);

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

fn start_stdin_to_socket(mut socket_writer: TcpStream, event_sender: Sender<RelayEvent>) {
    thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();

            /*
             * Zed already sends standard LSP framing. Forward this direction
             * without parsing or modifying it.
             */
            io::copy(&mut stdin, &mut socket_writer)?;

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
) {
    thread::spawn(move || {
        let result: io::Result<()> = {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();

            /*
             * tool4d emits Content-Length without necessarily including a
             * space after the colon. Normalize the header before sending it
             * to Zed.
             */
            normalize_lsp_stream(
                &mut socket_reader,
                &mut stdout,
            )
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

/// Reads LSP frames from `input` and writes them to `output`.
///
/// The JSON-RPC body is copied byte-for-byte. Only the LSP header is
/// normalized. For example:
///
/// ```text
/// Content-Length:1655\r\n\r\n
/// ```
///
/// becomes:
///
/// ```text
/// Content-Length: 1655\r\n\r\n
/// ```
///
/// Additional headers are preserved.
pub fn normalize_lsp_stream<R, W>(mut input: R, mut output: W) -> io::Result<()>
where
    R: Read,
    W: Write,
{
    loop {
        let Some(header) = read_lsp_header(&mut input)? else {
            // EOF before another frame begins is a clean socket shutdown.
            output.flush()?;
            return Ok(());
        };

        let parsed_header = parse_lsp_header(&header)?;

        if parsed_header.content_length > MAX_LSP_BODY_SIZE {
            return Err(invalid_data(format!(
                "LSP body length {} exceeds the maximum of {} bytes",
                parsed_header.content_length, MAX_LSP_BODY_SIZE
            )));
        }

        write!(
            output,
            "Content-Length: {}\r\n",
            parsed_header.content_length
        )?;

        for additional_header in parsed_header.additional_headers {
            output.write_all(&additional_header)?;
            output.write_all(b"\r\n")?;
        }

        output.write_all(b"\r\n")?;

        let body_length = u64::try_from(parsed_header.content_length)
            .map_err(|_| invalid_data("LSP body length is too large"))?;

        let mut body = input.by_ref().take(body_length);
        let copied = io::copy(&mut body, &mut output)?;

        if copied != body_length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "LSP body was truncated: expected {body_length} bytes, \
                     received {copied}"
                ),
            ));
        }

        /*
         * Flush after each frame so that initialization responses and other
         * messages are delivered to Zed immediately.
         */
        output.flush()?;
    }
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

    let lines = header.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let last_line_index = lines.len() - 1;

    for (index, raw_line) in lines.into_iter().enumerate() {
        /*
         * read_lsp_header removes the final CRLF-CRLF delimiter. As a result:
         *
         * - every header line except the final one still ends in CR;
         * - the final header line has no trailing CR.
         */
        let line = if index == last_line_index {
            if raw_line.ends_with(b"\r") {
                return Err(invalid_data(
                    "unexpected carriage return at the end of LSP header",
                ));
            }

            raw_line
        } else {
            raw_line.strip_suffix(b"\r").ok_or_else(|| {
                invalid_data("LSP header lines must end with CRLF")
            })?
        };

        if line.is_empty() {
            return Err(invalid_data(
                "unexpected empty line inside LSP header",
            ));
        }

        let colon_position = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(|| {
                invalid_data("LSP header line has no colon")
            })?;

        let name = trim_ascii_whitespace(&line[..colon_position]);
        let value =
            trim_ascii_whitespace(&line[colon_position + 1..]);

        if name.is_empty() {
            return Err(invalid_data("LSP header name is empty"));
        }

        if name.eq_ignore_ascii_case(b"Content-Length") {
            if content_length.is_some() {
                return Err(invalid_data(
                    "duplicate Content-Length header",
                ));
            }

            let value = std::str::from_utf8(value).map_err(|_| {
                invalid_data(
                    "Content-Length contains non-UTF-8 bytes",
                )
            })?;

            if value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid_data(
                    "Content-Length is not a decimal integer",
                ));
            }

            let length = value.parse::<usize>().map_err(|_| {
                invalid_data("Content-Length is out of range")
            })?;

            content_length = Some(length);
        } else {
            /*
             * Preserve additional header lines without their line endings.
             * The canonical CRLF is restored when the header is written.
             */
            additional_headers.push(line.to_vec());
        }
    }

    let content_length = content_length.ok_or_else(|| {
        invalid_data("missing Content-Length header")
    })?;

    Ok(ParsedHeader {
        content_length,
        additional_headers,
    })
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
/// This helper retains raw byte-relay behavior and is intended for generic
/// transport tests. Use `normalize_lsp_stream` when testing tool4d output
/// framing.
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

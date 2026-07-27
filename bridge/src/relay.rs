use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    thread,
};

/// Relays the process's stdin/stdout to an established TCP connection.
///
/// Standard output is reserved exclusively for LSP protocol data. All
/// diagnostics must be written to standard error.
pub fn stdio_to_tcp(stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;

    let mut socket_reader = stream.try_clone()?;
    let mut socket_writer = stream;

    thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();

            io::copy(&mut stdin, &mut socket_writer)?;

            match socket_writer.shutdown(Shutdown::Write) {
                Ok(()) => Ok(()),

                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    Ok(())
                }

                Err(error) => Err(error),
            }
        })();

        if let Err(error) = result {
            eprintln!("tool4d-lsp-stdio: stdin-to-socket relay failed: {error}");
        }
    });

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    io::copy(&mut socket_reader, &mut stdout)?;
    stdout.flush()
}

/// Relays an arbitrary input/output pair to an established TCP connection.
///
/// This function is intended for automated tests and reuse by other clients.
pub fn streams_to_tcp<R, W>(mut input: R, mut output: W, stream: TcpStream) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write,
{
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

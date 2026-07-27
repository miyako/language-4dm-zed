use std::{
    cmp::Ordering as CmpOrdering,
    env,
    ffi::OsStr,
    fs,
    io::{self, BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, ExitCode, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand};
use tool4d_lsp_stdio::relay::{Relay, RelayEvent};
use wait_timeout::ChildExt;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[derive(Debug, Parser)]
#[command(
    name = "tool4d-lsp-stdio",
    version,
    about = "Bridge a stdio LSP client to the tool4d TCP language server"
)]
struct Cli {
    #[command(subcommand)]
    command: BridgeCommand,
}

#[derive(Debug, Subcommand)]
enum BridgeCommand {
    /// Start tool4d and relay its TCP connection over stdin/stdout.
    Launch {
        /// Path to the tool4d executable.
        ///
        /// If omitted, the adapter searches PATH, 4D Analyzer's VS Code storage,
        /// and conventional platform-specific locations.
        #[arg(long, env = "TOOL4D_PATH")]
        tool: Option<PathBuf>,

        /// Explicit path to a .4DProject file.
        #[arg(long, env = "TOOL4D_PROJECT")]
        project: Option<PathBuf>,

        /// Workspace in which to search for a .4DProject file.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Local TCP port on which the adapter listens for tool4d.
        ///
        /// If omitted, the operating system selects an available port.
        #[arg(long, env = "TOOL4D_LSP_PORT")]
        port: Option<u16>,

        /// Number of seconds to wait for tool4d to connect.
        #[arg(long, env = "TOOL4D_STARTUP_TIMEOUT", default_value_t = 30)]
        startup_timeout: u64,

        /// Number of seconds to wait before force-killing tool4d.
        #[arg(long, env = "TOOL4D_SHUTDOWN_TIMEOUT", default_value_t = 5)]
        shutdown_timeout: u64,

        /// Prevent execution of project startup database methods.
        ///
        /// Enabled by default. Override with
        /// TOOL4D_SKIP_ONSTARTUP=false or --skip-onstartup=false.
        #[arg(
            long,
            env = "TOOL4D_SKIP_ONSTARTUP",
            default_value_t = true,
            action = ArgAction::Set
        )]
        skip_onstartup: bool,

        /// Open the project without a data file.
        ///
        /// Enabled by default. Override with TOOL4D_DATALESS=false or
        /// --dataless=false.
        #[arg(
            long,
            env = "TOOL4D_DATALESS",
            default_value_t = true,
            action = ArgAction::Set
        )]
        dataless: bool,

        /// Diagnostic log level passed to tool4d.
        #[arg(long, env = "TOOL4D_LOG_LEVEL")]
        log_level: Option<String>,
    },

    /// Connect stdin/stdout to an already-listening TCP service.
    ///
    /// This mode is generic and is not used by the tool4d launcher.
    Connect {
        /// Address of the existing TCP service.
        #[arg(long)]
        address: SocketAddr,
    },
}

/// Owns the tool4d process and terminates it when dropped.
///
/// This provides cleanup during normal returns, propagated errors, and Rust
/// panics when panic unwinding is enabled.
struct ChildGuard {
    child: Child,
    shutdown_timeout: Duration,
}

impl ChildGuard {
    fn new(child: Child, shutdown_timeout: Duration) -> Self {
        Self {
            child,
            shutdown_timeout,
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn terminate(&mut self) {
        terminate_child(&mut self.child, self.shutdown_timeout);
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,

        Err(error) => {
            // Standard output is reserved exclusively for LSP data.
            eprintln!("tool4d-lsp-stdio: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        BridgeCommand::Launch {
            tool,
            project,
            workspace,
            port,
            startup_timeout,
            shutdown_timeout,
            skip_onstartup,
            dataless,
            log_level,
        } => launch(
            tool.as_deref(),
            project.as_deref(),
            workspace.as_deref(),
            port,
            Duration::from_secs(startup_timeout),
            Duration::from_secs(shutdown_timeout),
            skip_onstartup,
            dataless,
            log_level.as_deref(),
        ),

        BridgeCommand::Connect { address } => {
            let cancellation = install_signal_handlers()?;

            let stream = TcpStream::connect(address)
                .with_context(|| format!("failed to connect to {address}"))?;

            let relay = Relay::start(stream).context("failed to start the TCP stream relay")?;

            supervise_relay(relay, None, &cancellation)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn launch(
    requested_tool: Option<&Path>,
    explicit_project: Option<&Path>,
    workspace: Option<&Path>,
    requested_port: Option<u16>,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
    skip_onstartup: bool,
    dataless: bool,
    log_level: Option<&str>,
) -> Result<()> {
    let tool = resolve_tool(requested_tool)?;
    let project = resolve_project(explicit_project, workspace)?;
    let cancellation = install_signal_handlers()?;

    /*
     * tool4d is the TCP client. Keep the listener bound while tool4d starts.
     * Binding port zero atomically selects and reserves an available port.
     */
    let listener = create_listener(requested_port)?;
    let listener_address = listener
        .local_addr()
        .context("failed to obtain the bridge listener address")?;

    let port = listener_address.port();

    let mut command = Command::new(&tool);

    command
        .arg(format!("--project={}", project.display()))
        .arg(format!("--lsp={port}"))
        .stdin(Stdio::null())
        /*
         * Child stdout must never be inherited because adapter stdout is the
         * LSP protocol channel.
         */
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if skip_onstartup {
        command.arg("--skip-onstartup");
    }

    if dataless {
        command.arg("--dataless");
    }

    if let Some(log_level) = log_level {
        command.arg(format!("--log-level={log_level}"));
    }

    configure_process_group(&mut command);

    eprintln!("tool4d-lsp-stdio: listening for tool4d on {listener_address}");

    log_command(&tool, command.get_args());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {}", tool.display()))?;

    if let Some(stdout) = child.stdout.take() {
        forward_tool4d_stdout(stdout);
    }

    let mut child = ChildGuard::new(child, shutdown_timeout);

    let stream = accept_with_timeout(&listener, &mut child, startup_timeout, &cancellation)?;

    // Only one tool4d connection is expected.
    drop(listener);

    let relay = Relay::start(stream).context("failed to start the LSP stream relay")?;

    supervise_relay(relay, Some(&mut child), &cancellation)
}

fn create_listener(requested_port: Option<u16>) -> Result<TcpListener> {
    let port = requested_port.unwrap_or(0);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| {
        if port == 0 {
            "failed to bind a local TCP listener".to_owned()
        } else {
            format!(
                "failed to bind local TCP port {port}; \
                     the port may already be in use"
            )
        }
    })?;

    listener
        .set_nonblocking(true)
        .context("failed to configure the local TCP listener")?;

    Ok(listener)
}

fn accept_with_timeout(
    listener: &TcpListener,
    child: &mut ChildGuard,
    timeout: Duration,
    cancellation: &AtomicBool,
) -> Result<TcpStream> {
    let started = Instant::now();

    loop {
        if cancellation.load(Ordering::SeqCst) {
            bail!("termination requested while waiting for tool4d");
        }

        match listener.accept() {
            Ok((stream, peer_address)) => {
                if !peer_address.ip().is_loopback() {
                    eprintln!(
                        "tool4d-lsp-stdio: rejected non-loopback \
                         connection from {peer_address}"
                    );
                    continue;
                }

                /*
                 * Accepted streams inherit the listener's nonblocking status
                 * on some platforms. The relay uses blocking I/O.
                 */
                stream
                    .set_nonblocking(false)
                    .context("failed to make the tool4d connection blocking")?;

                stream
                    .set_nodelay(true)
                    .context("failed to configure the tool4d connection")?;

                eprintln!(
                    "tool4d-lsp-stdio: tool4d connected from \
                     {peer_address}"
                );

                return Ok(stream);
            }

            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}

            Err(error) => {
                return Err(error).context("failed while accepting the tool4d connection");
            }
        }

        if let Some(status) = child
            .try_wait()
            .context("failed to inspect the tool4d process")?
        {
            bail!("tool4d exited before connecting to the bridge: {status}");
        }

        if started.elapsed() >= timeout {
            bail!(
                "timed out after {} seconds waiting for tool4d to connect",
                timeout.as_secs()
            );
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn supervise_relay(
    relay: Relay,
    mut child: Option<&mut ChildGuard>,
    cancellation: &AtomicBool,
) -> Result<()> {
    const SUPERVISOR_INTERVAL: Duration = Duration::from_millis(100);

    loop {
        if cancellation.load(Ordering::SeqCst) {
            eprintln!(
                "tool4d-lsp-stdio: termination requested; \
                 stopping the LSP session"
            );

            relay.shutdown();
            return Ok(());
        }

        if let Some(child_guard) = child.as_deref_mut()
            && let Some(status) = child_guard
                .try_wait()
                .context("failed to inspect the tool4d process")?
        {
            relay.shutdown();

            if status.success() {
                return Ok(());
            }

            bail!("tool4d exited unexpectedly: {status}");
        }

        match relay.events().recv_timeout(SUPERVISOR_INTERVAL) {
            Ok(RelayEvent::StdinClosed) => {
                /*
                 * Zed has closed the adapter's input. Do not wait
                 * indefinitely for tool4d to close the TCP connection.
                 */
                eprintln!(
                    "tool4d-lsp-stdio: editor input closed; \
                     stopping tool4d"
                );

                relay.shutdown();
                return Ok(());
            }

            Ok(RelayEvent::SocketClosed) => {
                relay.shutdown();
                return Ok(());
            }

            Ok(RelayEvent::Error { direction, error }) => {
                relay.shutdown();

                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::NotConnected
                ) {
                    eprintln!(
                        "tool4d-lsp-stdio: {direction} relay closed: \
                         {error}"
                    );
                    return Ok(());
                }

                return Err(error).with_context(|| format!("{direction} LSP relay failed"));
            }

            Err(RecvTimeoutError::Timeout) => {}

            Err(RecvTimeoutError::Disconnected) => {
                relay.shutdown();
                bail!("both LSP relay workers stopped unexpectedly");
            }
        }
    }
}

fn install_signal_handlers() -> Result<Arc<AtomicBool>> {
    let cancellation = Arc::new(AtomicBool::new(false));

    #[cfg(unix)]
    {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

        for signal in [SIGINT, SIGTERM, SIGHUP] {
            signal_hook::flag::register(signal, Arc::clone(&cancellation))
                .with_context(|| format!("failed to register signal handler {signal}"))?;
        }
    }

    /*
     * On non-Unix systems this still returns a cancellation flag. Native
     * Windows console and Job Object support should be added before Windows
     * is declared a supported deployment platform.
     */

    Ok(cancellation)
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        /*
         * Put tool4d into a new process group. This lets shutdown terminate
         * tool4d and any processes it starts in the same group.
         */
        command.process_group(0);
    }

    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

fn terminate_child(child: &mut Child, shutdown_timeout: Duration) {
    match child.try_wait() {
        Ok(Some(_)) => return,

        Ok(None) => {}

        Err(error) => {
            eprintln!(
                "tool4d-lsp-stdio: failed to inspect tool4d during \
                 shutdown: {error}"
            );
        }
    }

    eprintln!("tool4d-lsp-stdio: terminating tool4d");

    send_graceful_termination(child);

    match child.wait_timeout(shutdown_timeout) {
        Ok(Some(_status)) => return,

        Ok(None) => {
            eprintln!(
                "tool4d-lsp-stdio: tool4d did not exit within {} \
                 seconds; forcing termination",
                shutdown_timeout.as_secs()
            );
        }

        Err(error) => {
            eprintln!(
                "tool4d-lsp-stdio: failed while waiting for tool4d: \
                 {error}"
            );
        }
    }

    force_terminate(child);

    if let Err(error) = child.wait() {
        eprintln!("tool4d-lsp-stdio: failed to reap tool4d: {error}");
    }
}

#[cfg(unix)]
fn send_graceful_termination(child: &mut Child) {
    let process_group = -(child.id() as libc::pid_t);

    // SAFETY: kill is called with a process-group identifier created for
    // this child. No Rust memory is accessed through the FFI call.
    let result = unsafe { libc::kill(process_group, libc::SIGTERM) };

    if result != 0 {
        let error = io::Error::last_os_error();

        if error.raw_os_error() != Some(libc::ESRCH) {
            eprintln!(
                "tool4d-lsp-stdio: failed to terminate tool4d process \
                 group: {error}"
            );
        }
    }
}

#[cfg(not(unix))]
fn send_graceful_termination(child: &mut Child) {
    if let Err(error) = child.kill() {
        eprintln!("tool4d-lsp-stdio: failed to terminate tool4d: {error}");
    }
}

#[cfg(unix)]
fn force_terminate(child: &mut Child) {
    let process_group = -(child.id() as libc::pid_t);

    // SAFETY: kill is called with a process-group identifier created for
    // this child. No Rust memory is accessed through the FFI call.
    let result = unsafe { libc::kill(process_group, libc::SIGKILL) };

    if result != 0 {
        let error = io::Error::last_os_error();

        if error.raw_os_error() != Some(libc::ESRCH) {
            eprintln!(
                "tool4d-lsp-stdio: failed to kill tool4d process \
                 group: {error}"
            );
        }
    }
}

#[cfg(not(unix))]
fn force_terminate(child: &mut Child) {
    if let Err(error) = child.kill() {
        eprintln!("tool4d-lsp-stdio: failed to kill tool4d: {error}");
    }
}

fn forward_tool4d_stdout(stdout: ChildStdout) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();

        loop {
            line.clear();

            match reader.read_until(b'\n', &mut line) {
                Ok(0) => return,

                Ok(_) => {
                    let stderr = io::stderr();
                    let mut stderr = stderr.lock();

                    if stderr
                        .write_all(b"tool4d-lsp-stdio: tool4d stdout: ")
                        .and_then(|_| stderr.write_all(&line))
                        .and_then(|_| {
                            if line.ends_with(b"\n") {
                                Ok(())
                            } else {
                                stderr.write_all(b"\n")
                            }
                        })
                        .and_then(|_| stderr.flush())
                        .is_err()
                    {
                        return;
                    }
                }

                Err(error) => {
                    eprintln!(
                        "tool4d-lsp-stdio: failed to read tool4d \
                         stdout: {error}"
                    );
                    return;
                }
            }
        }
    });
}

fn log_command<'a>(executable: &Path, arguments: impl Iterator<Item = &'a OsStr>) {
    let rendered_arguments = arguments
        .map(|argument| format!("{argument:?}"))
        .collect::<Vec<_>>()
        .join(" ");

    eprintln!(
        "tool4d-lsp-stdio: executing {} {}",
        executable.display(),
        rendered_arguments
    );
}

#[derive(Debug)]
struct ToolCandidate {
    path: PathBuf,
    version: (u64, u64),
    build: u64,
    modified: SystemTime,
}

fn resolve_tool(requested_tool: Option<&Path>) -> Result<PathBuf> {
    if let Some(tool) = requested_tool {
        let tool = canonicalize_tool(tool)?;

        eprintln!(
            "tool4d-lsp-stdio: using configured tool4d at {}",
            tool.display()
        );

        return Ok(tool);
    }

    if let Some(tool) = find_tool_on_path() {
        let tool = canonicalize_tool(&tool)?;

        eprintln!(
            "tool4d-lsp-stdio: using tool4d from PATH at {}",
            tool.display()
        );

        return Ok(tool);
    }

    if let Some(tool) = discover_vscode_analyzer_tool()? {
        let tool = canonicalize_tool(&tool)?;

        eprintln!(
            "tool4d-lsp-stdio: using 4D Analyzer tool4d at {}",
            tool.display()
        );

        return Ok(tool);
    }

    if let Some(tool) = discover_conventional_tool()? {
        let tool = canonicalize_tool(&tool)?;

        eprintln!(
            "tool4d-lsp-stdio: using installed tool4d at {}",
            tool.display()
        );

        return Ok(tool);
    }

    bail!(
        "could not find tool4d\n\
         \n\
         Searched:\n\
         - --tool\n\
         - TOOL4D_PATH\n\
         - the process PATH\n\
         - 4D Analyzer's VS Code global storage\n\
         - conventional platform-specific application locations\n\
         \n\
         Configure it explicitly with:\n\
         \n\
         tool4d-lsp-stdio launch --tool /path/to/tool4d ...\n\
         \n\
         or set TOOL4D_PATH"
    )
}

fn canonicalize_tool(tool: &Path) -> Result<PathBuf> {
    validate_tool(tool)?;

    tool.canonicalize()
        .with_context(|| format!("failed to resolve {}", tool.display()))
}

fn find_tool_on_path() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;

    for directory in env::split_paths(&path) {
        for executable_name in tool_executable_names() {
            let candidate = directory.join(executable_name);

            if is_valid_tool_candidate(&candidate) {
                return Some(candidate);
            }
        }
    }

    None
}

fn tool_executable_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["tool4d.exe"]
    }

    #[cfg(not(windows))]
    {
        &["tool4d"]
    }
}

fn discover_vscode_analyzer_tool() -> Result<Option<PathBuf>> {
    let mut roots = Vec::new();

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_directory() {
            let application_support = home.join("Library").join("Application Support");

            roots.push(
                application_support
                    .join("Code")
                    .join("User")
                    .join("globalStorage")
                    .join("4d.4d-analyzer")
                    .join("tool4d"),
            );

            roots.push(
                application_support
                    .join("Code - Insiders")
                    .join("User")
                    .join("globalStorage")
                    .join("4d.4d-analyzer")
                    .join("tool4d"),
            );

            roots.push(
                application_support
                    .join("VSCodium")
                    .join("User")
                    .join("globalStorage")
                    .join("4d.4d-analyzer")
                    .join("tool4d"),
            );
        }
    }

    #[cfg(windows)]
    {
        if let Some(app_data) = env::var_os("APPDATA") {
            let app_data = PathBuf::from(app_data);

            roots.push(
                app_data
                    .join("Code")
                    .join("User")
                    .join("globalStorage")
                    .join("4d.4d-analyzer")
                    .join("tool4d"),
            );

            roots.push(
                app_data
                    .join("Code - Insiders")
                    .join("User")
                    .join("globalStorage")
                    .join("4d.4d-analyzer")
                    .join("tool4d"),
            );

            roots.push(
                app_data
                    .join("VSCodium")
                    .join("User")
                    .join("globalStorage")
                    .join("4d.4d-analyzer")
                    .join("tool4d"),
            );
        }
    }

    let mut candidates = Vec::new();

    for root in roots {
        collect_analyzer_candidates(&root, &root, 0, &mut candidates)?;
    }

    candidates.sort_by(compare_tool_candidates);

    Ok(candidates.pop().map(|candidate| candidate.path))
}

fn collect_analyzer_candidates(
    root: &Path,
    directory: &Path,
    depth: usize,
    candidates: &mut Vec<ToolCandidate>,
) -> Result<()> {
    const MAX_DEPTH: usize = 8;
    const MAX_CANDIDATES: usize = 100;

    if depth > MAX_DEPTH || !directory.is_dir() {
        return Ok(());
    }

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,

        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }

        Err(error) => {
            return Err(error).with_context(|| format!("failed to search {}", directory.display()));
        }
    };

    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read an entry in {}", directory.display()))?;

        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;

        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            collect_analyzer_candidates(root, &path, depth + 1, candidates)?;

            continue;
        }

        if !file_type.is_file() || !is_tool_executable_name(&path) {
            continue;
        }

        if !is_valid_tool_candidate(&path) {
            continue;
        }

        let (version, build) = analyzer_version_and_build(root, &path);

        let modified = path
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        candidates.push(ToolCandidate {
            path,
            version,
            build,
            modified,
        });

        if candidates.len() > MAX_CANDIDATES {
            bail!(
                "more than {MAX_CANDIDATES} tool4d executables were found \
                 under {}",
                root.display()
            );
        }
    }

    Ok(())
}

fn analyzer_version_and_build(root: &Path, executable: &Path) -> ((u64, u64), u64) {
    let relative = match executable.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) => return ((0, 0), 0),
    };

    let components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();

    let version = components
        .first()
        .and_then(|value| parse_4d_version(value))
        .unwrap_or((0, 0));

    let build = components
        .get(1)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    (version, build)
}

fn parse_4d_version(value: &str) -> Option<(u64, u64)> {
    let uppercase = value.to_ascii_uppercase();

    if let Some((major, release)) = uppercase.split_once('R') {
        return Some((major.parse::<u64>().ok()?, release.parse::<u64>().ok()?));
    }

    Some((uppercase.parse::<u64>().ok()?, 0))
}

fn compare_tool_candidates(left: &ToolCandidate, right: &ToolCandidate) -> CmpOrdering {
    left.version
        .cmp(&right.version)
        .then_with(|| left.build.cmp(&right.build))
        .then_with(|| left.modified.cmp(&right.modified))
        .then_with(|| left.path.cmp(&right.path))
}

fn is_tool_executable_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };

    tool_executable_names()
        .iter()
        .any(|expected| name.eq_ignore_ascii_case(expected))
}

fn is_valid_tool_candidate(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn discover_conventional_tool() -> Result<Option<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        let mut application_directories = vec![PathBuf::from("/Applications")];

        if let Some(home) = home_directory() {
            application_directories.push(home.join("Applications"));
        }

        let mut candidates = Vec::new();

        for applications in application_directories {
            collect_macos_application_candidates(&applications, &mut candidates)?;
        }

        candidates.sort_by(|left, right| {
            left.file_name()
                .cmp(&right.file_name())
                .then_with(|| left.cmp(right))
        });

        Ok(candidates.pop())
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
fn collect_macos_application_candidates(
    applications: &Path,
    candidates: &mut Vec<PathBuf>,
) -> Result<()> {
    if !applications.is_dir() {
        return Ok(());
    }

    let entries = match fs::read_dir(applications) {
        Ok(entries) => entries,

        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(());
        }

        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to search {}", applications.display()));
        }
    };

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if !name.starts_with("4D") || !entry.path().is_dir() {
            continue;
        }

        let candidate = entry
            .path()
            .join("tool4d.app")
            .join("Contents")
            .join("MacOS")
            .join("tool4d");

        if is_valid_tool_candidate(&candidate) {
            candidates.push(candidate);
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn home_directory() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn validate_tool(tool: &Path) -> Result<()> {
    if !tool.exists() {
        bail!("tool4d does not exist: {}", tool.display());
    }

    if !tool.is_file() {
        bail!("tool4d path is not a file: {}", tool.display());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = tool
            .metadata()
            .with_context(|| format!("failed to inspect {}", tool.display()))?
            .permissions()
            .mode();

        if mode & 0o111 == 0 {
            bail!("tool4d is not executable: {}", tool.display());
        }
    }

    Ok(())
}

fn resolve_project(explicit_project: Option<&Path>, workspace: Option<&Path>) -> Result<PathBuf> {
    if let Some(project) = explicit_project {
        let project = if project.is_absolute() {
            project.to_path_buf()
        } else {
            let workspace = workspace.context("a relative --project path requires --workspace")?;

            workspace.join(project)
        };

        return validate_project(&project);
    }

    let workspace =
        workspace.context("no 4D project was supplied; use --project or --workspace")?;

    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }

    let mut projects = Vec::new();

    find_projects(workspace, 0, &mut projects)?;

    projects.sort();
    projects.dedup();

    match projects.len() {
        0 => {
            bail!("no .4DProject file was found under {}", workspace.display());
        }

        1 => Ok(projects.remove(0)),

        _ => {
            let project_list = projects
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");

            bail!(
                "multiple .4DProject files were found; \
                 use --project explicitly:\n{project_list}"
            );
        }
    }
}

fn validate_project(project: &Path) -> Result<PathBuf> {
    if !project.is_file() {
        bail!(
            "4D project does not exist or is not a file: {}",
            project.display()
        );
    }

    if !has_4d_project_extension(project) {
        bail!(
            "project path does not end in .4DProject: {}",
            project.display()
        );
    }

    project
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", project.display()))
}

fn has_4d_project_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("4DProject"))
}

fn find_projects(directory: &Path, depth: usize, projects: &mut Vec<PathBuf>) -> Result<()> {
    const MAX_DEPTH: usize = 6;
    const MAX_PROJECTS: usize = 100;

    if depth > MAX_DEPTH {
        return Ok(());
    }

    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?;

    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read an entry under {}", directory.display()))?;

        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;

        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            if should_ignore_directory(&entry.file_name()) {
                continue;
            }

            find_projects(&path, depth + 1, projects)?;
            continue;
        }

        if file_type.is_file() && has_4d_project_extension(&path) {
            let canonical_path = path
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", path.display()))?;

            projects.push(canonical_path);

            if projects.len() > MAX_PROJECTS {
                bail!("more than {MAX_PROJECTS} .4DProject files were found");
            }
        }
    }

    Ok(())
}

fn should_ignore_directory(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".zed" | "node_modules" | "target" | "build" | "dist")
    )
}

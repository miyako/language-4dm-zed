use std::{
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tool4d_lsp_stdio::relay;

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
        #[arg(long, env = "TOOL4D_PATH")]
        tool: PathBuf,

        /// Explicit path to a .4DProject file.
        #[arg(long, env = "TOOL4D_PROJECT")]
        project: Option<PathBuf>,

        /// Workspace in which to search for a .4DProject file.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Local TCP port on which to listen for tool4d.
        ///
        /// If omitted, the operating system chooses an available port.
        #[arg(long, env = "TOOL4D_LSP_PORT")]
        port: Option<u16>,

        /// Number of seconds to wait for tool4d to connect.
        #[arg(long, default_value_t = 30)]
        startup_timeout: u64,
    },

    /// Connect stdin/stdout to an already-listening TCP service.
    ///
    /// This generic mode is not used when launching tool4d, because tool4d is
    /// itself a TCP client.
    Connect {
        /// Address of the existing TCP service.
        #[arg(long)]
        address: SocketAddr,
    },
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
        } => launch(
            &tool,
            project.as_deref(),
            workspace.as_deref(),
            port,
            Duration::from_secs(startup_timeout),
        ),

        BridgeCommand::Connect { address } => {
            let stream = TcpStream::connect(address)
                .with_context(|| format!("failed to connect to {address}"))?;

            relay::stdio_to_tcp(stream).context("the TCP stream relay failed")
        }
    }
}

fn launch(
    tool: &Path,
    explicit_project: Option<&Path>,
    workspace: Option<&Path>,
    requested_port: Option<u16>,
    startup_timeout: Duration,
) -> Result<()> {
    validate_tool(tool)?;

    let project = resolve_project(explicit_project, workspace)?;

    /*
     * tool4d is the TCP client. The bridge must therefore bind and retain the
     * listening socket before launching tool4d.
     */
    let listener = create_listener(requested_port)?;
    let listener_address = listener
        .local_addr()
        .context("failed to obtain the bridge listener address")?;

    let port = listener_address.port();

    let project_argument = format!("--project={}", project.display());
    let lsp_argument = format!("--lsp={port}");

    eprintln!("tool4d-lsp-stdio: listening for tool4d on {listener_address}");

    eprintln!(
        "tool4d-lsp-stdio: executing {} {:?} {:?}",
        tool.display(),
        project_argument,
        lsp_argument
    );

    let mut child = Command::new(tool)
        .arg(&project_argument)
        .arg(&lsp_argument)
        .stdin(Stdio::null())
        /*
         * Adapter stdout belongs to Zed's LSP transport. Never allow child
         * process output to enter it.
         */
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", tool.display()))?;

    let result = match accept_with_timeout(&listener, &mut child, startup_timeout) {
        Ok(stream) => {
            // Only one connection is needed.
            drop(listener);

            relay::stdio_to_tcp(stream).context("the LSP stream relay failed")
        }

        Err(error) => Err(error),
    };

    stop_child(&mut child);

    result
}

fn create_listener(requested_port: Option<u16>) -> Result<TcpListener> {
    let port = requested_port.unwrap_or(0);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| {
        if port == 0 {
            "failed to bind a local TCP listener".to_owned()
        } else {
            format!("failed to bind local TCP port {port}")
        }
    })?;

    listener
        .set_nonblocking(true)
        .context("failed to configure the local TCP listener")?;

    Ok(listener)
}

fn accept_with_timeout(
    listener: &TcpListener,
    child: &mut Child,
    timeout: Duration,
) -> Result<TcpStream> {
    let started = Instant::now();

    loop {
        match listener.accept() {
            Ok((stream, peer_address)) => {
                if !peer_address.ip().is_loopback() {
                    bail!("rejected a non-loopback connection from {peer_address}");
                }

                // The listener is nonblocking so that we can monitor the child process
                // and enforce a startup timeout. The established LSP stream must be
                // switched back to blocking mode before io::copy is used.
                stream
                    .set_nonblocking(false)
                    .context("failed to make the tool4d connection blocking")?;

                stream
                    .set_nodelay(true)
                    .context("failed to configure the tool4d connection")?;

                eprintln!("tool4d-lsp-stdio: tool4d connected from {peer_address}");

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

fn validate_tool(tool: &Path) -> Result<()> {
    if !tool.exists() {
        bail!("tool4d does not exist: {}", tool.display());
    }

    if !tool.is_file() {
        bail!("tool4d path is not a file: {}", tool.display());
    }

    Ok(())
}

fn resolve_project(explicit_project: Option<&Path>, workspace: Option<&Path>) -> Result<PathBuf> {
    if let Some(project) = explicit_project {
        return validate_project(project);
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

fn should_ignore_directory(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".zed" | "node_modules" | "target" | "build" | "dist")
    )
}

fn stop_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_status)) => return,

        Ok(None) => {}

        Err(error) => {
            eprintln!(
                "tool4d-lsp-stdio: failed to inspect tool4d during \
                 shutdown: {error}"
            );
        }
    }

    if let Err(error) = child.kill() {
        eprintln!("tool4d-lsp-stdio: failed to terminate tool4d: {error}");
    }

    if let Err(error) = child.wait() {
        eprintln!("tool4d-lsp-stdio: failed to wait for tool4d: {error}");
    }
}

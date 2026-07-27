use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
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
    about = "Bridge stdio LSP clients to the tool4d TCP language server"
)]
struct Cli {
    #[command(subcommand)]
    command: BridgeCommand,
}

#[derive(Debug, Subcommand)]
enum BridgeCommand {
    /// Start tool4d and connect stdio to its TCP language server.
    Launch {
        /// Path to the tool4d executable.
        #[arg(long, env = "TOOL4D_PATH")]
        tool: PathBuf,

        /// Explicit path to a .4DProject file.
        #[arg(long, env = "TOOL4D_PROJECT")]
        project: Option<PathBuf>,

        /// Workspace in which to find a .4DProject file.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Time to wait for the tool4d TCP server.
        #[arg(long, default_value_t = 30)]
        startup_timeout: u64,
    },

    /// Connect stdio to an already-running TCP language server.
    Connect {
        #[arg(long)]
        address: SocketAddr,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // stdout belongs exclusively to LSP.
            eprintln!("tool4d-lsp-stdio: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        BridgeCommand::Connect { address } => {
            let stream = TcpStream::connect(address)
                .with_context(|| format!("failed to connect to {address}"))?;

            relay::stdio_to_tcp(stream)?;
        }

        BridgeCommand::Launch {
            tool,
            project,
            workspace,
            startup_timeout,
        } => {
            launch(
                &tool,
                project.as_deref(),
                workspace.as_deref(),
                Duration::from_secs(startup_timeout),
            )?;
        }
    }

    Ok(())
}

fn launch(
    tool: &Path,
    explicit_project: Option<&Path>,
    workspace: Option<&Path>,
    startup_timeout: Duration,
) -> Result<()> {
    validate_executable(tool)?;

    let project = resolve_project(explicit_project, workspace)?;

    let port = choose_port()?;
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    eprintln!(
        "tool4d-lsp-stdio: starting {} for {} on {}",
        tool.display(),
        project.display(),
        address
    );

    let mut child = Command::new(tool)
        .arg(format!("--project={}", project.display()))
        .arg(format!("--lsp={port}"))
        .stdin(Stdio::null())
        // Do not inherit stdout because adapter stdout is the LSP channel.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", tool.display()))?;

    let result = connect_with_retry(address, &mut child, startup_timeout)
        .and_then(|stream| relay::stdio_to_tcp(stream).map_err(Into::into));

    stop_child(&mut child);

    result
}

fn validate_executable(tool: &Path) -> Result<()> {
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

    let workspace = workspace.context("no project was supplied; use --project or --workspace")?;

    let mut projects = Vec::new();
    find_projects(workspace, workspace, 0, &mut projects)?;

    match projects.len() {
        0 => bail!("no .4DProject file was found under {}", workspace.display()),
        1 => Ok(projects.remove(0)),
        _ => {
            let list = projects
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");

            bail!("multiple .4DProject files were found; use --project:\n{list}")
        }
    }
}

fn validate_project(project: &Path) -> Result<PathBuf> {
    if !project.is_file() {
        bail!("4D project does not exist: {}", project.display());
    }

    let valid_suffix = project
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("4DProject"));

    if !valid_suffix {
        bail!(
            "the project path does not end in .4DProject: {}",
            project.display()
        );
    }

    project
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", project.display()))
}

fn find_projects(
    root: &Path,
    directory: &Path,
    depth: usize,
    projects: &mut Vec<PathBuf>,
) -> Result<()> {
    const MAX_DEPTH: usize = 6;

    if depth > MAX_DEPTH {
        return Ok(());
    }

    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("cannot read {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            let name = entry.file_name();

            if matches!(name.to_str(), Some(".git" | "node_modules" | "target")) {
                continue;
            }

            find_projects(root, &path, depth + 1, projects)?;
        } else if file_type.is_file() {
            let is_project = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("4DProject"));

            if is_project {
                projects.push(path.canonicalize()?);
            }
        }
    }

    projects.sort();
    projects.dedup();

    // Prevent an unexpectedly large search result from consuming resources.
    if projects.len() > 100 {
        bail!(
            "more than 100 .4DProject files were found under {}",
            root.display()
        );
    }

    Ok(())
}

fn choose_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("failed to allocate a loopback port")?;

    let port = listener.local_addr()?.port();

    // tool4d must bind the port itself.
    drop(listener);

    Ok(port)
}

fn connect_with_retry(
    address: SocketAddr,
    child: &mut Child,
    timeout: Duration,
) -> Result<TcpStream> {
    let started = Instant::now();
    let retry_delay = Duration::from_millis(100);

    loop {
        if let Some(status) = child.try_wait()? {
            bail!("tool4d exited before opening the LSP socket: {status}");
        }

        match TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(stream);
            }

            Err(error) if started.elapsed() < timeout => {
                eprintln!("tool4d-lsp-stdio: waiting for {address}: {error}");
                thread::sleep(retry_delay);
            }

            Err(error) => {
                bail!("timed out waiting for tool4d at {address}: {error}");
            }
        }
    }
}

fn stop_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(error) => {
            eprintln!("tool4d-lsp-stdio: could not inspect tool4d: {error}");
        }
    }

    if let Err(error) = child.kill() {
        eprintln!("tool4d-lsp-stdio: could not terminate tool4d: {error}");
    }

    if let Err(error) = child.wait() {
        eprintln!("tool4d-lsp-stdio: could not wait for tool4d: {error}");
    }
}

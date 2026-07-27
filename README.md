# language-4dm-zed

The successful GUI test proves the basic architecture, but the current implementation is **not quite deployment-ready**. The main remaining concern is process lifecycle management.

# 1. Defensive-code review

## Current status

| Area | Status | Comment |
|---|---:|---|
| TCP listener created before `tool4d` | Good | Correct connection direction |
| Automatic port allocation | Good | Binding port `0` is race-free |
| Loopback-only listener | Good | Not exposed to the network |
| LSP stdout isolation | Good | Adapter logs go to stderr |
| Project validation | Good | Explicit project paths are checked |
| Multiple project detection | Good | Does not silently select one |
| Startup timeout | Good | Prevents indefinite waiting before connection |
| Child cleanup on normal relay exit | Partial | `stop_child()` runs on ordinary return |
| Cleanup on `SIGTERM`/`SIGINT` | Insufficient | Default signal handling can bypass cleanup |
| Cleanup if Zed closes stdin | Partial | Depends on `tool4d` closing its socket |
| Cleanup after panic | Insufficient | A panic can bypass `stop_child()` |
| Cleanup after adapter `SIGKILL` | Inherently limited | The process cannot run cleanup after `SIGKILL` |
| Project startup safety | Needs attention | Opening a project may execute startup methods |

## Do we risk an orphaned `tool4d` process?

Yes, under some conditions.

The current flow calls:

```rust
stop_child(&mut child);
```

after the relay returns. That covers:

- normal server disconnect;
- ordinary relay errors;
- startup timeout;
- `tool4d` exiting early.

It does not reliably cover:

- Zed terminating the adapter with `SIGTERM`;
- the user pressing Control-C;
- a Rust panic;
- Zed or the operating system killing the adapter with `SIGKILL`;
- the adapter blocking forever after stdin closes.

### Specific relay problem

When Zed closes adapter stdin, the stdin relay performs:

```rust
socket_writer.shutdown(Shutdown::Write)
```

The main thread remains blocked reading from the TCP socket until `tool4d` closes its side. If `tool4d` does not close after receiving EOF, both processes can remain alive.

## Required hardening before release

### A. Add a child-process guard

The child should be owned by an RAII guard whose `Drop` implementation terminates it. This covers most ordinary errors and panics.

Conceptually:

```rust
struct ChildGuard {
    child: Option<Child>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            terminate_child(child);
        }
    }
}
```

This does not solve `SIGKILL`, because no code executes after `SIGKILL`.

### B. Handle termination signals

On macOS/Linux, handle at least:

- `SIGINT`
- `SIGTERM`
- `SIGHUP`

The handler should initiate cancellation, close the TCP stream, and terminate `tool4d`.

Avoid doing complex process operations directly inside a low-level signal handler. Set an atomic cancellation flag or notify the main supervisor.

### C. Put `tool4d` in a dedicated process group

On Unix, launch `tool4d` in a separate process group. On shutdown, terminate the whole group rather than only the immediate child.

That matters if `tool4d` launches helper processes.

On Windows, use a Job Object configured with:

```text
JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
```

### D. Add a post-connection inactivity/shutdown strategy

The supervisor should react to any of these events:

- Zed stdin closes;
- `tool4d` closes TCP;
- `tool4d` exits;
- adapter receives a termination signal;
- one relay direction reports an error.

After one event occurs:

1. close both TCP directions;
2. allow `tool4d` a short grace period;
3. terminate it;
4. wait;
5. force-kill it if necessary.

The current two-thread `io::copy` implementation is suitable for proving transport, but a production supervisor is easier to implement with asynchronous I/O and cancellation, for example Tokio.

### E. Use `--skip-onstartup` by default

This is both a reliability and security issue.

Opening a 4D project may execute startup database methods. An editor extension should not automatically execute project startup behavior merely because a source file was opened.

The adapter should normally launch:

```text
tool4d
--project=<project>
--lsp=<port>
--skip-onstartup
```

Investigate whether 4D Analyzer also uses:

```text
--dataless
```

If LSP analysis does not require a data file, `--dataless` should probably be the default. It reduces locking, startup overhead, and side effects.

Provide explicit adapter options if users need to change these defaults:

```text
--run-startup-methods
--with-data
```

The safe behavior should be the default.

### F. Forward `tool4d` stdout to adapter stderr

Currently:

```rust
.stdout(Stdio::null())
```

protects LSP stdout, which is correct, but discards useful diagnostics.

A production implementation should pipe child stdout and copy it to adapter stderr with a prefix:

```text
tool4d-lsp-stdio: tool4d stdout: ...
```

It must never forward child output to adapter stdout.

---

# 2. What happens if the port is already used?

## Automatic port selection

The default behavior should remain:

```rust
TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
```

Port `0` asks the operating system to select an available port and bind it atomically.

Because the listener remains open while `tool4d` starts, there is no check-then-bind race.

This is the safest option.

## Explicit port

If the user supplies:

```bash
--port 19876
```

and that port is already occupied, this call fails before `tool4d` is launched:

```rust
TcpListener::bind((Ipv4Addr::LOCALHOST, 19876))
```

The current error should resemble:

```text
tool4d-lsp-stdio: failed to bind local TCP port 19876:
Address already in use (os error 48)
```

That is acceptable. The adapter should not silently choose a different port when the user explicitly requested one.

## Local connection race

Only loopback clients can connect, and an automatically selected port is disclosed directly to the newly launched `tool4d`. The risk of another local process connecting first is low but not mathematically zero.

For deployment, reject non-loopback connections as the code already does. Peer-process verification is platform-specific and probably unnecessary for the first release.

---

# 3. More user-friendly configuration

Environment variables are useful for testing, but they should not be the primary release interface.

## Recommended configuration priority

The adapter should resolve the `tool4d` executable in this order:

1. Explicit `--tool` argument.
2. Zed language-server binary arguments.
3. `TOOL4D_PATH`.
4. `tool4d` found on `PATH`.
5. Platform-specific discovery.
6. A clear error listing the paths checked.

## Automatic macOS discovery

The adapter can look for:

```text
/Applications/4D */tool4d.app/Contents/MacOS/tool4d
$HOME/Applications/4D */tool4d.app/Contents/MacOS/tool4d
```

If exactly one supported installation is found, use it.

If multiple versions are found, either:

- select the highest version and log the choice; or
- require explicit configuration.

Requiring explicit selection is more predictable for the first release.

## Zed settings

Use Zed’s standard language-server settings rather than requiring users to modify shell startup files.

The desired user-facing configuration is approximately:

```json
{
  "lsp": {
    "tool4d": {
      "binary": {
        "arguments": [
          "launch",
          "--tool",
          "/Applications/4D 21.1/tool4d.app/Contents/MacOS/tool4d"
        ]
      }
    }
  }
}
```

The extension should append:

```text
--workspace <worktree-root>
```

unless a project/workspace argument was already provided.

Before implementing this, compile against the currently pinned `zed_extension_api` and use its documented `LspSettings::for_worktree` API. Do not assume arbitrary custom settings are exposed to WebAssembly extensions.

A project containing multiple `.4DProject` files could use workspace-local Zed settings:

```text
.zed/settings.json
```

with:

```json
{
  "lsp": {
    "tool4d": {
      "binary": {
        "arguments": [
          "launch",
          "--tool",
          "/Applications/4D 21.1/tool4d.app/Contents/MacOS/tool4d",
          "--project",
          "/absolute/path/to/MyProject.4DProject"
        ]
      }
    }
  }
}
```

Absolute project paths are not portable. A later adapter version should accept paths relative to the worktree.

## Recommended bridge configuration

Support these options:

```text
--tool <path>
--project <path>
--workspace <path>
--port <number>
--startup-timeout <seconds>
--shutdown-timeout <seconds>
--skip-onstartup
--dataless
--log-level <level>
```

Defaults:

```text
port:             operating-system selected
startup timeout:  30 seconds
shutdown timeout: 5 seconds
skip-onstartup:   enabled
dataless:         verify against 4D Analyzer before enabling
```

---

# 4. Native-adapter deployment

A published Zed extension should not require users to clone the repository and run `cargo install`.

The recommended release model is:

```text
GitHub Release
├── tool4d-lsp-stdio-aarch64-apple-darwin.tar.gz
├── tool4d-lsp-stdio-x86_64-apple-darwin.tar.gz
├── tool4d-lsp-stdio-x86_64-pc-windows-msvc.zip
└── SHA256SUMS
```

Only publish platforms supported by `tool4d`.

The Zed extension should:

1. determine the platform and architecture;
2. choose the matching versioned release artifact;
3. download it through the supported Zed extension API;
4. extract it into the extension-managed directory;
5. mark it executable where required;
6. return it as the language-server command;
7. cache it by adapter version.

Do not redistribute `tool4d`. Users must install 4D separately.

## Release CI

Add a GitHub Actions workflow that runs:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

for the native bridge.

The extension crate should run:

```bash
cargo fmt --check
cargo check
cargo build --target wasm32-wasip1
```

Release artifacts should be built from a tag such as:

```text
v0.1.0
```

and accompanied by SHA-256 checksums.

---

# 5. Proposed repository documentation

Recommended files:

```text
README.md
LICENSE
docs/
├── architecture.md
├── configuration.md
├── development.md
├── troubleshooting.md
└── security.md
```

No images are required for the initial documentation. Later, screenshots of these would improve the published extension:

1. a `.4dm` file with syntax highlighting;
2. a completion popup;
3. hover or diagnostics;
4. the Zed Extensions page showing the 4D extension.

---

# 6. Proposed `README.md`

```markdown
# 4D for Zed

4D language support for [Zed](https://zed.dev/).

This extension provides:

- `.4dm` language recognition;
- Tree-sitter parsing and syntax highlighting;
- Language Server Protocol support using `tool4d`;
- a native adapter that converts Zed's standard-input/standard-output
  LSP transport to the TCP transport used by 4D.

## Requirements

- Zed
- 4D or `tool4d` 21 or later
- macOS

Additional platforms will be documented when corresponding `tool4d`
and adapter builds have been verified.

## Architecture

Zed starts language servers as subprocesses and communicates using
standard input and output.

`tool4d` uses TCP and connects to the port supplied with `--lsp`.

The native adapter connects these transports:

```text
Zed stdin/stdout
       |
       v
tool4d-lsp-stdio
       ^
       | TCP connection to a loopback listener
       |
tool4d --project=<project> --lsp=<port>
```

The adapter creates the TCP listener before launching `tool4d`.
`tool4d` is the TCP client, while it remains the LSP server at the
protocol level.

## Installation for development

### Install Rust

Install Rust through `rustup`:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup target add wasm32-wasip1
```

Make sure the rustup-managed compiler is used:

```sh
command -v rustc
rustc --print sysroot
```

### Build and install the adapter

From this repository:

```sh
cargo fmt --manifest-path bridge/Cargo.toml

cargo test \
  --manifest-path bridge/Cargo.toml

cargo build \
  --manifest-path bridge/Cargo.toml \
  --release

cargo install \
  --path bridge \
  --locked \
  --force
```

Verify the installation:

```sh
command -v tool4d-lsp-stdio
tool4d-lsp-stdio --version
```

### Build the Zed extension

```sh
cargo check
cargo build --target wasm32-wasip1
```

### Install the development extension

1. Open Zed.
2. Open the command palette.
3. Run `zed: install dev extension`.
4. Select this repository's root directory, which contains
   `extension.toml`.
5. Open a `.4dm` file.

The language selector should display `4D`.

## Manual adapter test

```sh
tool4d-lsp-stdio launch \
  --tool "/Applications/4D 21.1/tool4d.app/Contents/MacOS/tool4d" \
  --project "/absolute/path/to/Project.4DProject"
```

Expected diagnostic output:

```text
tool4d-lsp-stdio: listening for tool4d on 127.0.0.1:<port>
tool4d-lsp-stdio: executing ...
tool4d-lsp-stdio: tool4d connected from 127.0.0.1:<peer-port>
```

All diagnostics are written to stderr. Standard output is reserved for
LSP messages.

## Configuration

During development, the `tool4d` executable can be supplied with:

```sh
export TOOL4D_PATH="/Applications/4D 21.1/tool4d.app/Contents/MacOS/tool4d"
```

The adapter searches the current Zed worktree for a `.4DProject` file.

- If exactly one project is found, it is used.
- If none are found, startup fails with an explanatory error.
- If multiple projects are found, configure one explicitly.

Environment variables are retained as a fallback. A future release will
use Zed's language-server settings and automatic `tool4d` discovery.

## Tree-sitter grammar

The extension uses:

<https://github.com/miyako/tree-sitter-fourd>

The grammar is pinned to a full commit SHA in `extension.toml`.

After updating the grammar:

1. generate and test the parser;
2. commit and push the grammar;
3. update the grammar revision in `extension.toml`;
4. copy updated query files into `languages/4d`;
5. rebuild the Zed development extension;
6. inspect Zed's log for query errors.

## Development checks

Zed extension:

```sh
cargo fmt --check
cargo check
cargo build --target wasm32-wasip1
```

Native adapter:

```sh
cargo fmt \
  --manifest-path bridge/Cargo.toml \
  --check

cargo clippy \
  --manifest-path bridge/Cargo.toml \
  --all-targets \
  -- \
  -D warnings

cargo test \
  --manifest-path bridge/Cargo.toml
```

## Troubleshooting

### `.4dm` appears as Unknown

Open Zed's log and look for a language-query error.

For example:

```text
Error loading highlights query
Invalid node type "..."
```

Every node or anonymous token referenced by a Tree-sitter query must
exist in the pinned grammar revision.

### `tool4d` cannot create a socket

The process passed to `--lsp` is a TCP client. A listener must already
exist before `tool4d` is launched.

Use `tool4d-lsp-stdio launch`; do not normally invoke `tool4d --lsp`
directly.

### Adapter not found

```sh
cargo install --path bridge --locked --force
command -v tool4d-lsp-stdio
```

Cargo normally installs it into `$HOME/.cargo/bin`.

## Security

The adapter listens only on the IPv4 loopback address.

The bridge never interprets or modifies LSP payloads. It forwards bytes
between Zed and `tool4d`.

Opening a 4D project can potentially invoke project startup behavior.
The release adapter will disable startup methods by default.

## License

See [LICENSE](LICENSE).
```

---

# 7. Zed publication protocol

The extension should be published only after native-adapter distribution is automatic or the manual dependency is clearly documented.

## Repository preparation

Before submission:

- `extension.toml` contains the stable ID `fourd`;
- version is correct;
- grammar revision is a full commit SHA;
- repository is public;
- repository contains a license;
- README describes installation and requirements;
- generated `extension.wasm` is not committed;
- Zed build directories and grammar checkout directories are ignored;
- the extension works from a fresh clone;
- the adapter release URL is versioned;
- no user-specific paths exist in source.

Suggested `.gitignore`:

```gitignore
/target/
/extension.wasm
/grammars/
/bridge/target/
.DS_Store
```

Do not ignore either `Cargo.lock` if reproducible application/extension builds are desired.

## Submit to Zed’s extension registry

Zed extensions are submitted through the official extensions repository:

```text
https://github.com/zed-industries/extensions
```

The expected flow is:

1. Fork `zed-industries/extensions`.
2. Clone the fork with submodules.
3. Add `language-4dm-zed` as a submodule under the registry’s extension directory.
4. Add the extension’s registry metadata using ID `fourd`.
5. Run every validation command required by that repository’s current `README` or `CONTRIBUTING.md`.
6. Commit the submodule and registry changes.
7. Push a branch to the fork.
8. Open a pull request against `zed-industries/extensions`.
9. Complete the repository’s current PR template.
10. Address automated and maintainer review.

The registry entry will normally have the conceptual form:

```toml
[fourd]
submodule = "extensions/fourd"
version = "0.1.0"
```

However, use the exact schema and commands in the current `zed-industries/extensions` repository at submission time. Its contribution tooling is authoritative.

## Updating after publication

For each update:

1. update and test `language-4dm-zed`;
2. increment `version` in `extension.toml`;
3. commit and push;
4. update the `fourd` submodule revision in the extensions registry;
5. update the registry version if required;
6. run registry validation;
7. open a registry update PR.

Do not reuse an existing version number for changed code.

---

# 8. Recommended order from here

1. Add safe defaults: `--skip-onstartup`, and investigate `--dataless`.
2. Replace the current relay lifecycle with cancellable supervision.
3. Add process-group termination and signal handling.
4. Add integration tests proving no orphan remains.
5. Add automatic `tool4d` discovery and Zed settings support.
6. Build signed/checksummed adapter artifacts in GitHub Actions.
7. Make the Zed extension download the correct adapter artifact.
8. Test installation from a clean macOS account.
9. Add the documentation above.
10. Capture optional screenshots.
11. Submit the extension-registry PR.

The most important release blocker is process supervision. A syntax-highlighting-only release could be published without the LSP adapter, but an LSP-enabled release should not ship until shutdown behavior has been tested under normal close, Zed quit, `SIGTERM`, adapter crash, and `tool4d` crash.

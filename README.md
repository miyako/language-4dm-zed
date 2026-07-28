# 4D for Zed

4D language support for [Zed](https://zed.dev/).

This extension provides:

- `.4dm` language recognition
- Tree-sitter parsing and syntax highlighting
- Language Server Protocol support using `tool4d`
- Native adapter that converts Zed's standard-input/standard-output LSP transport to TCP transport used by 4D.

## Requirements

- Zed
- `tool4d` 21 or later
- macOS (tested on Apple Silicon) Windows and Intel Mac support are experimental

## Architecture

Zed starts language servers as subprocesses and communicates using standard input and output.

`tool4d` uses TCP and connects to the port supplied with `--lsp`.

The native adapter connects these transports:

```text
Zed stdin/stdout
       |
       |
tool4d-lsp-stdio
       |
       | TCP connection to a loopback listener
       |
tool4d --project=<project> --lsp=<port>
```

## Language Server 

A native adapter corresponsing to the Zed platform is downloaded from GitHub assets:

| Zed platform | Asset |
|---|---|
| macOS ARM64 | `tool4d-lsp-stdio-aarch64-apple-darwin.zip` |
| macOS Intel | `tool4d-lsp-stdio-x86_64-apple-darwin.zip` |
| Windows ARM64 | `tool4d-lsp-stdio-aarch64-pc-windows-msvc.zip` |
| Windows x64 | `tool4d-lsp-stdio-x86_64-pc-windows-msvc.zip` |

Restart Zed and open a 4D workspace or project that contains `.4dm` files.

# Environment variables

| Variable | Default | Purpose |
|---|---:|---|
| `TOOL4D_PATH` | required currently | Path to `tool4d` |
| `TOOL4D_PROJECT` | auto-discovered | Explicit `.4DProject` |
| `TOOL4D_LSP_PORT` | OS-selected | Fixed bridge listener port |
| `TOOL4D_STARTUP_TIMEOUT` | `30` | Connection timeout in seconds |
| `TOOL4D_SHUTDOWN_TIMEOUT` | `5` | Graceful shutdown timeout |
| `TOOL4D_SKIP_ONSTARTUP` | `true` | Suppress startup methods |
| `TOOL4D_DATALESS` | `true` | Open without a data file |
| `TOOL4D_LOG_LEVEL` | unset | Passed to `tool4d --log-level` |

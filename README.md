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
- `tool4d` 21 or later
- macOS or Windows

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

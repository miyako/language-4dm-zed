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
- macOS (apple Silicon)

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
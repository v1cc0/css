# css

**English** | [简体中文](https://github.com/v1cc0/css/blob/main/README.zh-CN.md) 

`css` is a small Rust CLI for discovering, inspecting, and resuming active
Codex CLI sessions on a Linux host.

The tool reads Codex session metadata from `~/.codex/state_5.sqlite`, matches it
with running `codex` processes in `/proc`, and can build or execute the matching
`codex resume` command.

## Features

- List currently running Codex CLI processes.
- Infer each process' saved Codex thread/session metadata.
- Show whether a session appears to be working, waiting for user input, or
  malfunctioning.
- Resume a session by session id, title, PID, or current working directory.
- Take over a running session by terminating the original process and resuming
  it in the current terminal.
- Optionally expose list/resolve/takeover operations through a local Unix socket
  daemon.

## Requirements

- Linux (`/proc` is used for process discovery)
- Rust toolchain
- Codex CLI installed and available as `codex`

## Build

```bash
cargo build --release
```

The compiled binary is written to:

```bash
target/release/css
```

## Usage

List running Codex sessions:

```bash
css list
```

Print JSON output:

```bash
css list --json
css status --json
```

Resume a session:

```bash
css continue <session-id>
css continue <pid>
css continue :cwd
```

Pass an initial prompt to the resumed session:

```bash
css continue <session-id> -- "continue the previous task"
```

Preview the resume command without executing it:

```bash
css continue <session-id> --dry-run
```

Take over a running session:

```bash
css takeover <pid>
css takeover :cwd
```

Run the local daemon:

```bash
css serve
css serve --socket /tmp/codexdaemon.sock
```

## Configuration

Use a custom Codex home directory:

```bash
css --codex-home /path/to/.codex list
```

Or set:

```bash
CODEX_HOME=/path/to/.codex css list
```

Use a custom Codex binary:

```bash
css --codex-bin /path/to/codex continue <session-id>
```

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE).

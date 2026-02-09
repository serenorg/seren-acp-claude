# seren-acp-claude

ACP (Agent Client Protocol) for Claude Code.

## What it is

`seren-acp-claude` speaks ACP JSON-RPC over stdio and internally:
- Spawns `claude` as a subprocess (via `claude-code-agent-sdk`)
- Translates ACP prompt turns to Claude Code queries
- Streams Claude output back to the client as ACP session updates
- Bridges Claude tool approvals through ACP `session/request_permission`
- Uses JSON-RPC 2.0 for the ACP transport (handled by `agent-client-protocol`)

## Architecture

```
┌────────────┐     stdio      ┌──────────────────┐     stdio      ┌──────────────────┐
│ ACP Client │ ─────────────► │ seren-acp-claude │ ─────────────► │ claude           │
│            │ ◄───────────── │                  │ ◄───────────── │ (subprocess)     │
└────────────┘   JSON-RPC     └──────────────────┘   JSON-RPC     └──────────────────┘
```

## Prerequisites

- Install Claude Code CLI (see https://docs.anthropic.com/claude-code/)
- Authenticate with Claude: `claude login`
- ACP spec: https://agentclientprotocol.com/

## Running

Note: this binary expects an ACP client to drive it over stdin/stdout. Running it directly will wait for JSON-RPC messages.

```bash
# Debug build
cargo run --bin seren-acp-claude

# Release build
cargo run --release --bin seren-acp-claude

# Enable logs
RUST_LOG=info cargo run --release --bin seren-acp-claude
RUST_LOG=debug cargo run --release --bin seren-acp-claude
```

## Session modes

| Mode ID | Meaning |
|--------:|---------|
| `ask` | Ask before running tools (default) |
| `auto` | Auto-approve safe operations |

## ACP support

**Methods**
- `initialize`
- `authenticate` (no-op; Claude CLI handles auth)
- `newSession`
- `prompt`
- `setSessionMode`
- `cancel`
- `loadSession` is not supported

**High-level event mapping**
- ACP session = Claude Code session
- ACP tool calls / permission prompts = Claude tool approvals

## Development

- MSRV: Rust `1.90` (see `Cargo.toml`)
- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`

## License

MIT

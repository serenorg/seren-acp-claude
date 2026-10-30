# Repository Guidelines

## Scope

- This repository owns `seren-acp-claude`, a stdio bridge that speaks the Agent Client Protocol (ACP) to a client and drives the Claude Code CLI (`claude`) as a subprocess. It translates ACP prompt turns into Claude queries, streams Claude output back as ACP session updates, and forwards Claude tool approvals as ACP `session/request_permission` requests.
- The ACP transport and types belong to the `agent-client-protocol` crate. Spawning and talking to the Claude CLI belongs to `claude-agent-sdk-rs`, pinned by git to `serenorg/claude-agent-sdk-rs`; fix SDK behavior there, then bump the pin in `Cargo.lock` here.
- Claude authentication belongs to the Claude CLI (`claude login`). ACP `authenticate` is a no-op here; do not add credential handling to this bridge.
- Treat ACP client input and Claude CLI output as untrusted. Convert external strings, such as session mode IDs, at one boundary into checked types and reject unknown values.

## Structure

- The binary entry point is `src/main.rs`, which calls `acp::run_acp_server`. The library root is `src/lib.rs`.
- `src/acp/server.rs` implements the ACP `Agent` trait: sessions, modes, models, prompts, cancellation, history replay and MCP server conversion. `src/acp/mod.rs` defines the outbound message channel to the ACP client.
- `src/claude/client.rs` wraps the Claude SDK client: connection, permission mode, queries, cancellation and permission prompts. Shared errors are in `src/error.rs`.
- Unit tests live in `#[cfg(test)]` modules beside the code, currently in `src/acp/server.rs`. There are no integration tests, CI workflows, Makefile, git hooks, deploy configuration or generated files.
- The crate targets Rust 2024 with an MSRV of 1.90, set in `Cargo.toml`. Build output goes to `target/`, which is ignored.

## Validation

- Run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before every commit. These are the checks `README.md` lists; there is no CI to run them for you.
- Use `cargo build --locked` to confirm the committed `Cargo.lock` still resolves. Do not change the SDK pin or other dependencies unless the change needs it.
- Test behavior through stable seams, such as the model and command parsers in `src/acp/server.rs`. Do not read source or documentation files to assert implementation text.
- Name tests after the behavior they prove, not an issue, incident, review or removed implementation.
- Tests must not require an installed or authenticated `claude` CLI, network access or the developer's `~/.claude` directory.
- Report the exact checks you ran and any remaining risks.

## Security and Operations

- The bridge reads Claude Code session files under `$HOME/.claude/projects/` to list, resume and replay sessions. They hold the user's conversations and tool payloads: never log, copy or commit their contents.
- Never commit Claude credentials, API keys, MCP server environment values or session transcripts. Keep secrets and conversation text out of `log` output at every level.
- The `bypassPermissions` mode auto-approves every Claude tool call. Keep it an explicit client choice; never make it a default or a fallback when a mode is invalid.
- stdout carries the ACP JSON-RPC stream. Write logs only through `log`/`env_logger` (stderr, controlled by `RUST_LOG`) and never print to stdout.
- This binary runs on the user's machine under an ACP client. It has no production service or deployment owned by this repository.

## Changes and Commits

- Keep changes scoped and preserve unrelated work. Follow existing patterns and avoid new dependencies without a clear reason.
- Use Conventional Commits with a concise single-line subject. Do not include issue numbers, ticket URLs, personal signatures, co-author trailers, generated-by notices or tool attribution.
- Do not record issue or pull request numbers, URLs or commit hashes as history in code, comments or test names. Keep comments to invariants and non-obvious constraints.
- Put a version bump in its own final commit with only `Cargo.toml` and the `Cargo.lock` changes it causes. Match the existing `vX.Y.Z` tag convention.
- Never bypass hooks with `git commit --no-verify`. Never push, delete or replace a remote ref without explicit authorization.

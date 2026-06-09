# agent-sync

`agent-sync` is a Rust CLI for synchronizing a user's own local agent tooling
between coding agents such as Codex and Claude Code. It is a personal tool that
can be reused by other people, not an organization-managed configuration bundle.

It is meant to be shared as a tool, not as a dump of one person's private
configuration. Each person runs it against their own `~/.codex`, `~/.claude`,
`~/.claude.json`, and optional `~/.agents` directories.

## What It Syncs

- Skills from Codex, Claude, and shared `.agents/skills`.
- User rules such as Codex `AGENTS.md` and Claude `CLAUDE.md`.
- MCP server definitions from Codex `config.toml` and Claude `.claude.json`.
- Codex memory and automation folders as Claude-side reference imports.

## What It Does Not Sync

- Raw secrets, tokens, passwords, or private authorization values.
- Runtime auth state for hosted connectors.
- Personal packs by default. A pack can contain private content, so treat packs
  like local backups unless you have intentionally curated them for sharing.
- Automation execution. Codex automations are copied as reference templates only.

## Install

From this directory:

```bash
cargo build --release
```

The binary is written to:

```bash
target/release/agent-sync
```

## Quick Start

Inspect the current machine without writing anything:

```bash
agent-sync discover
```

Export the current user's local agent tooling into a neutral pack:

```bash
agent-sync export --pack ./my-agent-pack --from all
```

Preview what would change:

```bash
agent-sync diff --pack ./my-agent-pack --targets codex,claude
```

Run the apply command in dry-run mode:

```bash
agent-sync apply --pack ./my-agent-pack --targets claude
```

Write the changes:

```bash
agent-sync apply --pack ./my-agent-pack --targets claude --yes
```

Verify the installed resources:

```bash
agent-sync verify --pack ./my-agent-pack --targets claude
```

## Reuse By Other People

There are two distinct workflows:

1. Share `agent-sync` itself with teammates so each person can sync their own
   local setup across their own agents.
2. Create a curated team pack only for non-sensitive shared assets, such as
   team skills, style rules, or standard MCP server names that use environment
   variables for credentials.

Do not commit a personal exported pack unless you have reviewed it and are sure
it contains no private memories, customer data, local paths, or personal rules.

## Publishing

This project is intended to live under a personal GitHub namespace unless it is
intentionally transferred later. Do not publish exported packs with the tool.

## Pack Format

An exported pack is a directory with:

```text
agent-sync.manifest.json
skills/
rules/
mcp/servers.json
references/
```

The manifest records resource names, source agent, target agents, pack paths,
and content hashes. The files inside the pack are the source of truth for apply
and verify operations.

## Safety Model

- `discover`, `status`, `diff`, and `apply` without `--yes` are read-only.
- `apply --yes` writes only the requested targets.
- Existing files are backed up under `~/.agent-sync/backups/<timestamp>/`.
- MCP definitions preserve credential references instead of copying raw secrets.
- Codex `env_http_headers` entries become Claude header values like
  `"${ENV_NAME}"`.
- Claude header values are exported only when they already use `"${ENV_NAME}"`
  syntax.

## Path Overrides

Use flags or environment variables when testing or running against alternate
homes:

```bash
agent-sync \
  --home /tmp/example \
  --codex-home /tmp/example/.codex \
  --claude-home /tmp/example/.claude \
  --claude-config /tmp/example/.claude.json \
  --agents-home /tmp/example/.agents \
  discover
```

Equivalent environment variables:

- `AGENT_SYNC_HOME`
- `AGENT_SYNC_CODEX_HOME`
- `AGENT_SYNC_CLAUDE_HOME`
- `AGENT_SYNC_CLAUDE_CONFIG`
- `AGENT_SYNC_AGENTS_HOME`

## Commands

```text
agent-sync init --pack <path>
agent-sync status [--pack <path>] [--targets codex,claude]
agent-sync discover [--format text|json]
agent-sync export --pack <path> [--from all|codex|claude]
agent-sync diff --pack <path> [--targets codex,claude]
agent-sync apply --pack <path> [--targets codex,claude] [--yes]
agent-sync verify --pack <path> [--targets codex,claude]
```

## Development

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

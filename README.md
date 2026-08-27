# agent-sync

`agent-sync` keeps personal agent configuration aligned across Codex, Claude
Code, and Cursor. It preserves target-owned configuration and never copies
authentication state or raw secrets.

## Use It Through Your Agent

The normal interface is natural language. Setup installs a bundled
`agent-sync` skill in `~/.agents/skills`, where Codex and Cursor can discover
it. If Claude Code is a configured target, the first sync also installs the
skill in Claude's skill directory.

After setup, ask your agent:

- "Keep my Codex setup in sync with Cursor."
- "Show me what agent-sync would change."
- "Is agent-sync healthy?"
- "Repair my Cursor chat indexing."

The skill uses the managed commands below. You do not need to remember the pack
workflow for routine use.

`agent-sync status` also states whether Cursor history and QMD refresh are
enabled. The bundled skill uses that line when you ask it to repair Cursor chat
indexing, including when history export exists but QMD refresh was disabled.

## Set Up Once

Preview the default setup:

```bash
agent-sync setup
```

The default route is Codex to Cursor. It excludes memories, automation
references, MCP servers, and Cursor history. It also blocks replacements by
default.

Review the preview, then save the same setup:

```bash
agent-sync setup --yes
```

Setup writes `~/.agent-sync/config.toml` and installs the bundled skill. It does
not run the first sync.

After the first setup, later `setup` commands patch only the options you name.
Unspecified source, target, MCP, history, reference, update, and health settings
stay unchanged.

To select MCP servers, name each reviewed server during both the preview and
the saved setup:

```bash
agent-sync setup --mcp-servers qmd,exa,figma
agent-sync setup --mcp-servers qmd,exa,figma --yes
```

No MCP servers are selected when `--mcp-servers` is absent. `--all-mcp` is
available for an intentional full import, but review the source first.

You can also choose another source or target:

```bash
agent-sync setup --from claude --to cursor
agent-sync setup --from claude --to cursor --yes
```

## Sync

Preview the current plan:

```bash
agent-sync sync
```

The preview does not change agent configuration or replace a failed applied-run
record. Apply and verify the plan with:

```bash
agent-sync sync --yes
```

Each applied sync exports a private temporary pack, checks the plan, maintains
the bundled natural-language skill, sweeps missed Cursor transcripts when
history is enabled, applies allowed changes, verifies the result, checks for
remaining drift, and removes the temporary pack.

Updates that replace target content stay blocked unless setup explicitly uses
`--allow-updates`. Cursor-owned resources remain preserved even when updates
are allowed.

## Check Status

Use the short status view for routine checks:

```bash
agent-sync status
```

It shows the managed route, current drift, last successful sync, health, and the
next action.

Use the detailed diagnostic view when status needs attention:

```bash
agent-sync doctor
```

Doctor checks the config, source and target paths, MCP policy, bundled skill,
Cursor history hook, QMD refresh and pending-export state, current drift, run
ledger consistency, and the last applied attempt and success. It does not repair
files. It exits nonzero when any required check fails.

## Schedule It

After a manual setup and successful sync, configure your scheduler to run this
one command:

```bash
agent-sync sync --yes --automation
```

Use a stable installed binary. If the scheduler has a limited `PATH`, use the
absolute binary path.

The command is the job payload. It does not create a schedule by itself.

Automation mode prints `DONT_NOTIFY` only when an applied run is healthy, has
no additions or updates, and finds no newly preserved conflict. Changes,
failures, and new conflicts produce a short report instead.

## Durable State

Managed configuration and run history live under `~/.agent-sync`:

```text
~/.agent-sync/config.toml
~/.agent-sync/state/last-attempt.json
~/.agent-sync/state/last-success.json
~/.agent-sync/state/runs.jsonl
~/.agent-sync/state/bundled-skill.json
~/.agent-sync/state/qmd-refresh-state.json
~/.agent-sync/state/qmd-pending/
~/.agent-sync/backups/<timestamp>/
```

`last-attempt.json` records applied successes and failures. A preview never
clears a failed attempt.
`last-success.json` changes only after an applied run passes verification.
`runs.jsonl` keeps the applied-run history. Backups are created before managed
files are replaced.

## Cursor Safety

Cursor sync is additive and preserves Cursor as the owner of its existing
configuration:

- Cursor reads Codex, Claude, and shared `.agents` skills from their live
  directories.
- A differing skill already under `~/.cursor/skills` is preserved.
- Codex guidance uses `~/.cursor/rules/imported-codex-agents.mdc` as a bridge to
  the live `~/.codex/AGENTS.md` file.
- A differing existing Cursor rule is preserved.
- Existing same-name Cursor MCP entries remain unchanged.
- MCP servers supplied by a Cursor project or plugin are preserved.
- Missing selected MCP entries are added without changing unrelated JSON
  fields or file permissions.
- A symlinked Cursor MCP file is refused.
- Cursor settings, plugins, chat databases, editor state, and authentication
  sessions are not copied.

## Cursor History And QMD

Enable searchable Cursor history during setup:

```bash
agent-sync setup --cursor-history
agent-sync setup --cursor-history --yes
agent-sync sync --yes
```

The applied sync installs or repairs one managed Cursor `stop` hook. The hook
exports user and assistant text to `~/Documents/Obsidian/sessions` and refreshes
the QMD index and embeddings after a completed agent run. Each applied sync also
sweeps Cursor's transcript directory before one QMD refresh, so the scheduled
job catches chats missed by the hook. It preserves unrelated hooks and does not
modify Cursor's chat database or export tool calls.

`agent-sync doctor` checks that the hook points to the current executable, the
QMD `sessions` collection includes Cursor Markdown, every local transcript has
a current export, the exports are retrievable from QMD, no embeddings are
pending, and a successful QMD refresh is recorded.

The Markdown copies are portable search history. They do not replace Cursor's
native chat history, `@Chats`, resume, fork, or shared-transcript features, and
they cannot resume a native Cursor conversation.

## What It Can Manage

`agent-sync` understands:

- Codex skills from `~/.codex/skills`.
- Claude skills from `~/.claude/skills`.
- Shared skills from `~/.agents/skills`.
- Codex guidance from `~/.codex/AGENTS.md`.
- Claude guidance from `~/.claude/CLAUDE.md`.
- MCP definitions from Codex, Claude, and Cursor.
- Optional Codex memory and automation references in advanced packs.
- Cursor user skills, rules, MCP names, and optional transcript exports.

It does not copy bearer tokens, API keys, passwords, private keys, hosted
connector sessions, browser sessions, plugin installation state, or team
authentication state.

This setup installs personal skills under `~/.agents/skills`, which local Cursor
and Codex sessions can discover. For Cloud Agents, remote machines, or a team
workflow, place the approved skills in the repository or install agent-sync on
that environment. Personal files on one Mac are not automatically present on
another worker.

MCP exports keep supported environment variable references instead of secret
values. For example:

```toml
[mcp_servers.example.env_http_headers]
Authorization = "EXAMPLE_MCP_AUTHORIZATION"
```

becomes:

```json
{
  "headers": {
    "Authorization": "${EXAMPLE_MCP_AUTHORIZATION}"
  }
}
```

Each user must provide the referenced environment variable on their own
machine.

## Install

Build with Rust and Cargo:

```bash
cargo build --release
```

The binary is written to `target/release/agent-sync`. Place it at a stable path
on your `PATH` before setup or scheduling.

## Advanced: Pack Commands

The managed workflow creates temporary packs for you. Use the low-level pack
commands for inspection, migration, or a curated shared pack.

The terms are:

- **Source**: the agent configuration to export. Values are `codex`, `claude`,
  or `all`.
- **Pack**: a local directory with exported resources and a manifest.
- **Target**: the agent configuration to inspect or update. Values are `codex`,
  `claude`, `cursor`, or a comma-separated combination.

Start with an inventory:

```bash
agent-sync discover
agent-sync discover --format json
```

Create and inspect a pack:

```bash
agent-sync init --pack ./my-agent-pack
agent-sync export --pack ./my-agent-pack --from codex
find ./my-agent-pack -maxdepth 3 -type f
cat ./my-agent-pack/agent-sync.manifest.json
```

Unlike managed setup, a low-level export includes every source MCP definition
when `--mcp-servers` is absent. Select names explicitly when you do not want a
full MCP export.

Preview, apply, and verify it:

```bash
agent-sync diff --pack ./my-agent-pack --targets cursor
agent-sync apply --pack ./my-agent-pack --targets cursor
agent-sync apply --pack ./my-agent-pack --targets cursor --yes
agent-sync verify --pack ./my-agent-pack --targets cursor
agent-sync diff --pack ./my-agent-pack --targets cursor
```

`apply` is a preview unless `--yes` is present. A final diff should contain no
addition or update.

For a portable Cursor pack with selected MCP servers:

```bash
agent-sync export \
  --pack ./codex-cursor-pack \
  --from codex \
  --portable-only \
  --mcp-servers qmd,exa,figma
```

`--portable-only` excludes memory and automation references. It refuses a
reused pack whose `references` directory is not empty.

A pack can contain private paths and context. Inspect it before sharing. Prefer
a curated pack that excludes memories, customer context, local-only rules, and
generated backups.

### Pack Format

```text
agent-sync.manifest.json
skills/
rules/
mcp/servers.json
references/
```

The manifest records each resource's kind, name, source agent, pack path,
content hash, and intended targets. Pack contents are the source of truth for
`diff`, `apply`, and `verify`.

### Manual Cursor History Hook

The managed workflow handles this hook when Cursor history is enabled. For a
manual installation:

```bash
agent-sync cursor-history install
agent-sync cursor-history install --yes
```

The first command previews the additive hook. The second command writes it.

### Path Overrides

Default paths are:

```text
~/.agent-sync/config.toml
~/.codex
~/.claude
~/.claude.json
~/.cursor
~/.cursor/mcp.json
~/.agents
```

Override them with command options:

```bash
agent-sync \
  --config /tmp/example/.agent-sync/config.toml \
  --home /tmp/example \
  --codex-home /tmp/example/.codex \
  --claude-home /tmp/example/.claude \
  --claude-config /tmp/example/.claude.json \
  --cursor-home /tmp/example/.cursor \
  --cursor-config /tmp/example/.cursor/mcp.json \
  --agents-home /tmp/example/.agents \
  discover
```

Equivalent environment variables are:

- `AGENT_SYNC_CONFIG`
- `AGENT_SYNC_HOME`
- `AGENT_SYNC_CODEX_HOME`
- `AGENT_SYNC_CLAUDE_HOME`
- `AGENT_SYNC_CLAUDE_CONFIG`
- `AGENT_SYNC_CURSOR_HOME`
- `AGENT_SYNC_CURSOR_CONFIG`
- `AGENT_SYNC_AGENTS_HOME`

## Development

Run the checks:

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The integration tests use temporary agent homes. They do not use the machine's
real Codex, Claude, or Cursor directories.

## Publishing

The tool can be public, but a raw personal pack may contain private context.
Review any shared pack like other public configuration.

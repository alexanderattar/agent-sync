# agent-sync

`agent-sync` is a Rust CLI for syncing your own local agent tooling between
coding agents like Codex and Claude Code.

The important part is "your own". This is not meant to be a shared dump of my
skills, memories, MCP config, or private setup. The tool can be shared, but each
person should run it against their own machine and their own agent directories.

## When To Use This

Use `agent-sync` when you have useful local agent setup in one place and want
another agent to have the same practical context.

Good examples:

- You have Codex skills and want Claude Code to see the same skills.
- You keep shared skills in `~/.agents/skills` and want both agents to use them.
- You have MCP server definitions in Codex or Claude and want the other agent to
  have matching server entries.
- You want to move Codex `AGENTS.md` guidance into Claude as an imported rule.
- You want a local backup-style pack of your own agent setup before changing
  machines.

Bad examples:

- Publishing your personal exported pack without reviewing it.
- Sharing your memories, customer context, private rules, local paths, or MCP
  auth details with a team by accident.
- Expecting this to copy hosted connector login state. It does not.
- Expecting Codex automations to run inside Claude. They are copied only as
  reference templates.

## What It Syncs

`agent-sync` currently understands these local resources:

- Codex skills from `~/.codex/skills`.
- Claude skills from `~/.claude/skills`.
- Shared skills from `~/.agents/skills`.
- Codex global/project guidance from `~/.codex/AGENTS.md`.
- Claude user guidance from `~/.claude/CLAUDE.md`.
- MCP servers from Codex `~/.codex/config.toml`.
- MCP servers from Claude `~/.claude.json`.
- Codex memories as Claude-side reference imports.
- Codex automations as reference templates in the exported pack.

Some conversions are intentionally conservative:

- Codex `AGENTS.md` can be imported into Claude as
  `~/.claude/rules/imported-codex-agents.md`.
- Claude `CLAUDE.md` is exported as a Claude-targeted rule. It is not rewritten
  into Codex guidance yet.
- Skills and MCP server definitions can target both Codex and Claude.
- Codex automations are exported into the pack but are not installed into
  Claude, because Claude does not have the same automation runtime.

## What It Does Not Sync

It does not copy raw secrets.

That means:

- No bearer tokens.
- No API keys.
- No passwords.
- No private keys.
- No hosted connector sessions.
- No browser login state.
- No team-wide auth state.

For MCP servers, the tool tries to preserve references to environment variables
instead of copying the secret values themselves.

For example, a Codex config like this:

```toml
[mcp_servers.example.env_http_headers]
Authorization = "EXAMPLE_MCP_AUTHORIZATION"
```

becomes a Claude MCP header like this:

```json
{
  "headers": {
    "Authorization": "${EXAMPLE_MCP_AUTHORIZATION}"
  }
}
```

That still means each user has to set `EXAMPLE_MCP_AUTHORIZATION` in their own
environment. The pack does not contain the value.

## Install

You need Rust and Cargo.

From the repo:

```bash
cargo build --release
```

The binary will be here:

```bash
target/release/agent-sync
```

You can either run it from that path or put it somewhere on your `PATH`.

## Terms

There are three concepts that matter:

- **Source**: where the pack is exported from. `codex`, `claude`, or `all`.
- **Pack**: a local directory containing the exported resources and manifest.
- **Target**: where the pack is applied. `codex`, `claude`, or both.

A pack is just files. Treat it like a backup of your agent setup. If your agent
setup contains private context, the pack probably does too.

## Safe First Run

Start read-only.

```bash
agent-sync discover
```

This prints what the tool can see in Codex, Claude, and shared `.agents`.

Then export a pack:

```bash
agent-sync export --pack ./my-agent-pack --from all
```

Before applying anything, inspect the pack:

```bash
find ./my-agent-pack -maxdepth 3 -type f
```

Look at the manifest:

```bash
cat ./my-agent-pack/agent-sync.manifest.json
```

Look at any copied rules, skills, memory references, and MCP definitions. This
is the point where you should catch private notes, local paths, or anything you
would not want another person to see.

Preview the changes:

```bash
agent-sync diff --pack ./my-agent-pack --targets claude
```

Run apply without `--yes` first. This is still a dry run:

```bash
agent-sync apply --pack ./my-agent-pack --targets claude
```

If the plan looks right, write it:

```bash
agent-sync apply --pack ./my-agent-pack --targets claude --yes
```

Then verify:

```bash
agent-sync verify --pack ./my-agent-pack --targets claude
```

After a successful apply, a second diff should be unchanged:

```bash
agent-sync diff --pack ./my-agent-pack --targets claude
```

## Common Workflows

### Bring Codex Setup Into Claude

This is probably the first useful workflow for most people.

```bash
agent-sync export --pack ./codex-pack --from codex
agent-sync diff --pack ./codex-pack --targets claude
agent-sync apply --pack ./codex-pack --targets claude
agent-sync apply --pack ./codex-pack --targets claude --yes
agent-sync verify --pack ./codex-pack --targets claude
```

What this can install into Claude:

- Codex skills.
- Shared `.agents` skills.
- Imported Codex `AGENTS.md` guidance.
- MCP server definitions.
- Codex memory files under `~/.claude/agent-sync-import/codex-memories`.

### Bring Claude Skills And MCP Into Codex

```bash
agent-sync export --pack ./claude-pack --from claude
agent-sync diff --pack ./claude-pack --targets codex
agent-sync apply --pack ./claude-pack --targets codex
agent-sync apply --pack ./claude-pack --targets codex --yes
agent-sync verify --pack ./claude-pack --targets codex
```

Current boundary: Claude `CLAUDE.md` is preserved as Claude guidance. It is not
automatically rewritten into Codex `AGENTS.md`.

### Use A Curated Shared Pack

This is the team-safe version.

Someone can create a pack that only includes shared, non-sensitive resources,
then commit that curated pack somewhere else. Do not start by committing a raw
personal export.

Before sharing a pack, check:

- No memory files with private or customer context.
- No local filesystem paths that point to one person's machine.
- No company-only instructions unless the repo is meant to be private.
- No raw MCP auth values.
- No personal preferences that should not become team defaults.
- No generated backups or `target/` output.

If in doubt, share the tool, not the pack.

## Backups And Recovery

`discover`, `status`, `diff`, and `apply` without `--yes` are read-only.

`apply --yes` writes files and creates backups first. Backups go here by default:

```text
~/.agent-sync/backups/<timestamp>/
```

If you pass `--home`, backups go under that home instead:

```text
<home>/.agent-sync/backups/<timestamp>/
```

For example:

```bash
agent-sync \
  --home /tmp/example \
  --codex-home /tmp/example/.codex \
  --claude-home /tmp/example/.claude \
  --claude-config /tmp/example/.claude.json \
  --agents-home /tmp/example/.agents \
  apply --pack ./my-agent-pack --targets claude --yes
```

That writes backups under:

```text
/tmp/example/.agent-sync/backups/<timestamp>/
```

Recovery is manual right now. Copy the backed-up file or directory back to its
original location.

## Pack Format

An exported pack looks like this:

```text
agent-sync.manifest.json
skills/
rules/
mcp/servers.json
references/
```

The manifest records:

- Resource kind.
- Resource name.
- Source agent.
- Pack path.
- Content hash.
- Intended targets.

The pack contents are the source of truth for `diff`, `apply`, and `verify`.

## Path Overrides

By default, the tool uses:

```text
~/.codex
~/.claude
~/.claude.json
~/.agents
```

For tests, alternate installs, or machine migrations, pass explicit paths:

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

Create an empty pack:

```bash
agent-sync init --pack ./my-agent-pack
```

Show local inventory, or show pack diff if `--pack` is provided:

```bash
agent-sync status
agent-sync status --pack ./my-agent-pack --targets codex,claude
```

Discover local agent resources:

```bash
agent-sync discover
agent-sync discover --format json
```

Export a pack:

```bash
agent-sync export --pack ./my-agent-pack --from all
agent-sync export --pack ./codex-pack --from codex
agent-sync export --pack ./claude-pack --from claude
```

Preview changes:

```bash
agent-sync diff --pack ./my-agent-pack --targets claude
agent-sync diff --pack ./my-agent-pack --targets codex,claude
```

Apply in dry-run mode:

```bash
agent-sync apply --pack ./my-agent-pack --targets claude
```

Apply for real:

```bash
agent-sync apply --pack ./my-agent-pack --targets claude --yes
```

Verify after applying:

```bash
agent-sync verify --pack ./my-agent-pack --targets claude
```

## Development

Run the usual checks:

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The integration tests use temporary agent homes. They should not touch your real
Codex or Claude directories.

## Publishing

This repo is a personal reusable tool. If you fork it, keep that distinction in
mind:

- The code can be public.
- Your raw exported pack may not be safe to publish.
- A curated shared pack should be reviewed like any other public config.

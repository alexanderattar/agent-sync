# agent-sync

`agent-sync` keeps local agent configuration aligned across Codex, Claude Code,
and Cursor. Choose one canonical setup, preview the route, and let the managed
sync handle drift in the background.

Target-owned configuration stays target-owned. `agent-sync` does not copy
logins, editor settings, plugins, or native chat databases. It rejects
recognizable secrets during portable exports. Cursor chat export is a separate,
off-by-default option.

## Quick Start

Install the latest macOS or Linux release:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/alexanderattar/agent-sync/releases/latest/download/agent-sync-installer.sh | sh
```

The installer checks the archive's SHA-256 checksum. When a current,
authenticated GitHub CLI is available, it also verifies signed build provenance
from the tagged release workflow. It installs `agent-sync` in `~/.local/bin`.
Run the same command again to upgrade. Set `AGENT_SYNC_INSTALL_DIR` to use
another directory.

The short command above trusts the installer that GitHub serves. To authenticate
the installer before running it, use a current GitHub CLI and this copy-paste
block:

```bash
(
  set -eu
  repository="alexanderattar/agent-sync"
  version="$(gh release view --repo "$repository" --json tagName --jq .tagName)"
  temp_dir="$(mktemp -d)"
  trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
  gh release download "$version" --repo "$repository" \
    --pattern agent-sync-installer.sh --dir "$temp_dir"
  gh attestation verify "$temp_dir/agent-sync-installer.sh" \
    --repo "$repository" \
    --signer-workflow "$repository/.github/workflows/release.yml" \
    --source-ref "refs/tags/$version" \
    --deny-self-hosted-runners
  sh "$temp_dir/agent-sync-installer.sh" \
    --version "$version" --require-attestation
)
```

This verifies the installer and the platform archive against the same tag and
release workflow. Run `gh auth login` first if GitHub CLI is not authenticated.
Without an authenticated GitHub CLI, the short installer prints a warning and
uses checksum verification only.

Then open the compact terminal interface:

```bash
~/.local/bin/agent-sync
```

Use bare `agent-sync` after adding `~/.local/bin` to `PATH`. The bundled
natural-language skill also finds the default absolute path, so your agent can
manage the setup even when your shell has not added it yet.

When a terminal is attached, the first run guides you through the source,
targets, selected MCP servers, and optional Cursor history. Later runs show the
managed route, drift, health, and useful actions. The UI collects your choices;
setup and schedule changes get a review screen before they are written.

All commands also work without the UI, which makes them suitable for scripts
and agents.

The managed background job currently supports macOS. Linux users get the same
TUI, natural-language control, previews, and manual sync, but no installed
background service yet. Linux release archives use the GNU targets and are
built on Ubuntu 22.04 for compatibility with Ubuntu 22.04 and newer
glibc-based systems.

## Use Natural Language

Saving setup installs the bundled control skill under `~/.agents/skills` for
Codex and Cursor and under `~/.claude/skills` for Claude Code. All three agents
can then manage the same setup through natural language.

Ask your preferred agent:

- "Set up agent sync for this machine."
- "Keep my agent configuration in sync."
- "Show me what agent-sync would change."
- "Is agent-sync healthy?"
- "Run agent-sync now."
- "Turn on daily background sync."
- "Repair my Cursor chat indexing."

The skill translates these requests into the managed workflow. Routine use
does not require memorized command lines.

## Safe Defaults

- CLI setup, sync, schedule installation, and schedule removal preview changes
  by default. Their write forms require `--yes`. TUI write actions are explicit,
  and setup and schedule changes have confirmation screens.
- Existing target content is preserved unless you explicitly enable updates.
- Existing Cursor-specific skills, rules, MCP entries, project configuration,
  editor settings, plugins, and sessions are not overwritten. The optional
  managed history hook is added or repaired without removing unrelated hooks.
- Selected MCP definitions keep environment-variable references. Raw bearer
  tokens, API keys, passwords, and private keys are rejected.
- Managed replacements create backups. Changed or unowned schedule files are
  treated as conflicts instead of being replaced or removed.
- A target that changes after preview is preserved. The sync stops and asks for
  a fresh preview instead of replacing the newer content.

## Managed Workflow

The default setup route is Codex to Cursor. It excludes memories, automation
references, MCP servers, and Cursor history, and it blocks replacements.

Use the UI for guided setup, or preview and save the default setup directly:

```bash
agent-sync setup
agent-sync setup --yes
```

Setup writes `~/.agent-sync/config.toml` and installs the natural-language
skill. It does not run the first sync. Later setup commands change only the
options you name.

For example, select reviewed MCP servers or choose another supported route:

```bash
agent-sync setup --mcp-servers qmd,exa,figma
agent-sync setup --mcp-servers qmd,exa,figma --yes

agent-sync setup --from claude --to cursor
agent-sync setup --from claude --to cursor --yes

agent-sync setup --from cursor --to codex,claude
agent-sync setup --from cursor --to codex,claude --yes
```

No MCP server is selected by default. `--all-mcp` intentionally selects every
source server and should be used only after review.

Preview a sync, then apply and verify the same plan:

```bash
agent-sync sync
agent-sync sync --yes
```

An applied sync exports a private temporary pack, validates it, applies allowed
changes, verifies the result, records the run, and removes the pack. When
Cursor history is enabled, it also maintains the history hook and catches
missed transcript exports.

## Status and Maintenance

Use the UI for the routine view or request text or JSON explicitly:

```bash
agent-sync
agent-sync status
agent-sync status --format json
```

Status includes the route, history mode, current drift, last successful sync,
health, and the next action. JSON output provides the same managed status for
scripts and other tools.

Use the detailed read-only diagnostic when status needs attention:

```bash
agent-sync doctor
```

Doctor checks configuration, source and target paths, MCP policy, the bundled
skill, Cursor history, QMD coverage, current drift, and the run ledger. It exits
nonzero when a required check fails. Unrelated QMD embedding work appears as a
warning; missing or pending `agent-sync` exports still require attention.

## Background Sync on macOS

After one successful manual sync, preview and install the managed daily
LaunchAgent:

```bash
agent-sync schedule status
agent-sync schedule install
agent-sync schedule install --yes
```

The job uses the installed binary's absolute path and runs `sync --yes
--automation` every 24 hours. It pins the effective agent and config paths from
setup, so custom locations keep working in the background. Logs are stored
under `~/.agent-sync/logs`.

Preview removal before applying it:

```bash
agent-sync schedule uninstall
agent-sync schedule uninstall --yes
```

The schedule owns only its managed LaunchAgent. It refuses to replace or remove
an unowned or locally modified file.

## Upgrade or Uninstall

Rerun either Quick Start method to replace the binary with the latest release.
The managed schedule uses the same stable binary path, so it does not need to
be recreated after a normal upgrade.

On macOS, disable any managed background job before removing the binary:

```bash
agent-sync schedule uninstall
agent-sync schedule uninstall --yes
```

Then remove the binary:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/alexanderattar/agent-sync/releases/latest/download/agent-sync-installer.sh | sh -s -- --uninstall
```

The installer removes only the binary. It preserves `~/.agent-sync`, synced
agent files, backups, and logs.

## Cursor History and QMD

Cursor chats are not indexed automatically unless you enable history export:

```bash
agent-sync setup --refresh-qmd
agent-sync setup --refresh-qmd --yes
agent-sync sync --yes
```

This is an explicit privacy choice. The applied sync installs or repairs one
managed Cursor `stop` hook. The hook copies user and assistant text into the
private `~/.agent-sync/history/cursor` directory, redacts recognizable secrets,
and queues a short background QMD refresh. The files use owner-only permissions
and are indexed in the dedicated `qmd://agent-sync-cursor/` collection.

Scheduled syncs catch transcripts missed by the hook. To keep private Markdown
copies without indexing them, use `--cursor-history` instead of
`--refresh-qmd`.

The hook preserves unrelated hooks. It does not modify Cursor's chat database
or export tool calls. The Markdown copies are portable search history, but they
do not replace Cursor's native history, `@Chats`, resume, fork, or sharing
features. Secret redaction is a safety layer, not a guarantee. Treat the export
directory as sensitive and enable this option only when you want searchable
chat copies. QMD also stores indexed text in its own local database, which has
QMD's permissions and retention behavior.

This covers local Cursor sessions that provide a transcript. Cursor cloud
agents do not load user-level `~/.cursor/hooks.json`, so cloud-only chats are
not exported by this workflow.

## What It Manages

`agent-sync` understands:

- Codex, Claude Code, Cursor, and shared personal skills.
- Codex guidance can be bridged additively into Claude Code and Cursor.
- Claude Code and Cursor-specific rules stay owned by their respective agent
  when their semantics do not map safely.
- Selected MCP definitions from the canonical source, applied additively to
  supported targets.
- Optional Codex memory and automation references in advanced packs.
- Optional Cursor transcript exports and QMD refresh.

Codex, Claude Code, or Cursor can be the canonical source. Any other supported
agent can be a target. Personal files are local to the machine; remote agents
and teammates need their own installation or approved repository-level skills.

MCP exports keep supported environment-variable references instead of secret
values. For example:

```toml
[mcp_servers.example.env_http_headers]
Authorization = "EXAMPLE_MCP_AUTHORIZATION"
```

becomes this in Claude Code:

```json
{
  "headers": {
    "Authorization": "${EXAMPLE_MCP_AUTHORIZATION}"
  }
}
```

and this in Cursor:

```json
{
  "headers": {
    "Authorization": "${env:EXAMPLE_MCP_AUTHORIZATION}"
  }
}
```

Each user must provide the referenced environment variable on their own
machine. Agent-specific connection and tool timeouts stay local. If a selected
MCP definition uses another option that cannot be represented safely in the
target agent, agent-sync stops and names that option instead of dropping it.

## Durable State

Managed configuration and run history live under `~/.agent-sync`:

```text
~/.agent-sync/config.toml
~/.agent-sync/state/last-attempt.json
~/.agent-sync/state/last-success.json
~/.agent-sync/state/runs.jsonl
~/.agent-sync/state/bundled-skill.json
~/.agent-sync/state/bundled-skill-claude.json
~/.agent-sync/state/cursor-mcp.json
~/.agent-sync/state/qmd-refresh-state.json
~/.agent-sync/state/qmd-pending/
~/.agent-sync/history/cursor/
~/.agent-sync/backups/<timestamp>/
~/.agent-sync/logs/
```

Previews do not alter run state. `last-success.json` changes only after an
applied run passes verification.

Only recorded, still-unmodified agent-sync MCP entries receive managed updates.
User and unknown Cursor entries remain preserved.

## Recovery

Ask any configured agent, "Help me recover the last agent-sync change." It
should start with `status` and `doctor`, inspect the recorded run and its backup
directory, and show the exact recovery plan before changing files.

`agent-sync` does not blindly restore a whole backup tree. A later agent run or
manual edit may own those paths now, so recovery stays reviewable and
path-specific. Failed schedule updates roll back automatically, and normal sync
repairs remain subject to the same preview, ownership, and verification checks.

## Build from Source

```bash
cargo build --release
```

The binary is written to `target/release/agent-sync`. Place it at a stable path
on your `PATH` before setup or scheduling.

## Release Integrity

The release workflow pins each GitHub Action to a full commit SHA. It publishes
a GitHub artifact attestation for the installer and each platform archive.
Consumers can verify the signer workflow, source tag, and use of GitHub-hosted
runners with `gh attestation verify`, as shown in Quick Start.

Before the first public tag, enable [GitHub release
immutability](https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/establish-provenance-and-integrity/prevent-release-changes).
This locks each published tag and its assets and adds GitHub's release
attestation. Build attestations prove which workflow produced an artifact. They
do not make a build reproducible or remove trust in repository maintainers,
GitHub-hosted runners, the Rust toolchain, or locked dependencies.

## Roll It Out to a Team

Publish a tagged release, then share the Quick Start command. Each teammate
runs `agent-sync`, chooses a source and targets, reviews the first sync, and can
enable daily maintenance on macOS. From then on, they can ask Codex, Claude
Code, or Cursor to check health, preview drift, sync, or repair the setup.

Keep personal authentication and local choices on each machine. Put shared team
instructions in version-controlled repository skills or rules. Use a reviewed,
new portable pack only when you need an explicit configuration handoff; do not
share a personal pack by default.

The same binary and bundled skill work with each supported local agent. Plugin
installations and account connections remain native to each agent;
`agent-sync` does not try to copy their credentials or sessions.

For a predictable rollout, pin `AGENT_SYNC_VERSION` in your internal setup
guide and test upgrades on one machine before changing the team pin. The public
installer verifies the release checksum and also verifies GitHub build
provenance when an authenticated, current GitHub CLI is available. Pass
`--require-attestation` when archive provenance must be mandatory. Use the
verified Quick Start block when the installer itself must also be authenticated.

## Advanced: Pack Commands

The managed workflow creates temporary packs for you. Use the low-level pack
commands for inspection, migration, or a curated shared pack.

The terms are:

- **Source**: the agent configuration to export. Values are `codex`, `claude`,
  `cursor`, or `all`.
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
reused or non-empty pack. Start each export with a new pack directory so stale
content cannot leak into the result.

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

## Sharing Packs

A raw personal pack can contain private paths and context. Review it like any
other public configuration before you publish or send it.

## License

MIT

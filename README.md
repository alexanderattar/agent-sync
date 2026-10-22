# agent-sync

Keep your personal skills, compatible instructions, and selected MCP server
definitions available when you switch between local coding agents.

Choose one source and sync it to your other agents. Review changes in the
terminal interface (TUI), then enable daily sync or ask your agent to run it.

Supports Codex, Claude Code, Cursor, and Amp on macOS and Linux.
**Amp support is currently on `main`; the latest release, v0.1.3, does not
include it.** To use Amp now, [build from source](docs/reference.md#build-from-source).

## Install and set up

Install the latest release:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/alexanderattar/agent-sync/releases/latest/download/agent-sync-installer.sh | sh
```

Then open the TUI:

```bash
~/.local/bin/agent-sync
```

The installer verifies the download checksum. For stricter verification, see
[installer verification](docs/reference.md#installer-verification).

1. Choose your **source**: the agent configuration you want to maintain.
2. Choose your **targets**: the other agents you want to keep aligned.
3. Select any MCP servers you want to copy. Leave Cursor history off unless
   you want searchable chat exports.
4. Review and save setup. Saving setup alone does not run the first sync.
5. Ask your configured agent: **“Preview and sync my agents.”** Review any
   conflicts it reports, then ask it to check sync health.

Reopen `agent-sync` whenever you want the TUI's status and actions. You can use
the absolute path above if `~/.local/bin` is not on your `PATH`.

## Everyday use: ask your agent

Setup installs a control skill so your configured local agents can manage
agent-sync through natural language. For example:

- “Preview and sync my agents.”
- “Is agent-sync healthy?”
- “Change which agents I sync.”

You do not need to memorize commands. If a running agent does not discover the
new skill, start a new session.

For a manual preview, apply, and health check:

```bash
agent-sync sync
agent-sync sync --yes
agent-sync status
```

CLI changes preview by default; `--yes` applies them. If health reports a problem,
ask your agent to diagnose it or run `agent-sync doctor`.

## Keep it running daily

After the first successful sync, ask your agent:

> Turn on daily background sync.

On macOS, this installs a local job that runs every 24 hours without an agent
session or model calls. Successful runs with no changes stay quiet; failures
request a macOS notification. Your Mac must be running and your user session
available for the job to run. Check health if you suspect a missed run.

Linux currently supports manual sync, but has no managed background service.
See [schedule details and removal](docs/reference.md#background-sync-on-macos).

## What syncs, and what stays local?

- **Syncs:** personal skills, instructions that map safely to the target, and
  explicitly selected MCP server definitions.
- **Stays local:** logins, secret values, plugins, custom subagent definitions,
  editor settings, native chat databases, and account or cloud memories.
  Optional advanced packs can carry Codex memory file references.
- **Preserves your changes:** target-owned files and conflicting settings are
  kept. Agent-sync records the files it manages and backs up applied changes.
  Preserved conflicts may need your review before both agents behave alike.

MCP configuration is not authentication. Each agent still needs its required
tools installed and accounts connected.

This is **one-source sync, not a two-way merge**. Working in a target does not
make it the new source or copy its new skills back automatically. For skills
you create from any agent, keep one maintained copy in
`~/.agents/skills/<name>/SKILL.md`, then sync.

These are local files. Installing agent-sync does not transfer your setup to
ChatGPT, Claude's web app, or remote agents.

## Optional: searchable Cursor chats

Ask your agent:

> Enable Cursor chat exports and QMD indexing.

This requires QMD installed locally. It creates private Markdown copies of
local Cursor transcripts and indexes them for search. It does not transfer
resumable chats between agents. Exports redact recognizable secrets, but
redaction is not a guarantee; treat exported chats as sensitive.

History export is off by default. See [privacy and indexing details](docs/reference.md#cursor-history-and-qmd).

## Share it with your team

Share this repository and the installation instructions. Each person chooses
their own source, targets, and account connections. Sharing the installer
does not share your personal configuration.

Distribute reviewed team skills through your team repository. Do not share
raw personal exports without checking them for private context and paths.
For a controlled rollout, [pin a release version](docs/reference.md#roll-it-out-to-a-team).

## More help

- [Upgrade or uninstall](docs/reference.md#upgrade-or-uninstall)
- [Recovery and backups](docs/reference.md#recovery)
- [Supported configuration and Amp details](docs/reference.md#what-it-manages)
- [Manual setup, packs, and custom paths](docs/reference.md#managed-workflow)
- [Build from source](docs/reference.md#build-from-source) and [development checks](docs/reference.md#development)

## License

MIT

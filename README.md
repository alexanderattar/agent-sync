# agent-sync

Keep your personal skills, compatible instructions, and selected MCP server
definitions available when you switch between local coding agents.

Choose one source and sync it to your other agents. Review changes in the
terminal interface (TUI), then enable daily sync or ask your agent to run it.

Supports Codex, Claude Code, Cursor, and Amp on macOS and Linux.
**Amp support is currently on `main`; the latest release, v0.1.3, does not
include it.** To use Amp now, build from source using the commands below.

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
4. Review and save setup, then review the first-sync preview.
5. Choose **Apply first sync** to apply and verify the changes, or **Finish
   later** to save setup without syncing. You can then ask your configured
   agent: **“Preview and sync my agents.”**

Use the arrow keys to move, Space to toggle selections, and Enter to continue.
The TUI guides you through:

```text
Choose source → Choose targets → Select MCP servers → Review setup
             → Preview first sync → Apply and verify
```

For example, choose Codex as your source and Claude Code and Cursor as targets.
After syncing, compatible skills from Codex are available in both targets.
Conflicting target-owned files stay unchanged and are reported for review.

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
In the TUI, select **Enable daily background sync** after your first successful
sync and confirm the preview. Or use these commands:

```bash
agent-sync schedule install          # Preview
agent-sync schedule install --yes    # Enable
agent-sync schedule status           # Check the job
agent-sync schedule uninstall --yes  # Disable
```

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
For a controlled rollout, pin the version in your team's install command:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/alexanderattar/agent-sync/releases/latest/download/agent-sync-installer.sh | sh -s -- --version v0.1.3
```

Test a new release on one machine before changing the team pin. The version
above does not include Amp support.

## If something looks wrong

Ask your agent: **“Check agent-sync health and explain anything that isn't
syncing.”** It can inspect the setup, preview changes, and diagnose failures.

- **A skill is missing:** check that it exists in your source or shared skills
  directory, then preview sync. A preserved conflict means the target has its
  own copy; review the difference rather than forcing an overwrite.
- **An MCP tool is unavailable:** check the selected server, local dependencies,
  environment variables, and that agent's authentication. Sync does not log in.
- **A daily run failed:** run `agent-sync doctor` and check the logs in
  `~/.agent-sync/logs`. Fix the reported issue, then preview and rerun sync.
- **You need to undo a change:** ask “Help me recover the last agent-sync
  change.” Backups live in `~/.agent-sync/backups`; review a file-specific
  recovery plan before restoring anything over newer edits.

## Upgrade or remove

Rerun the installation command to upgrade. Your configuration and synced files
remain in place, and the daily job keeps using the installed binary path.

To remove agent-sync, disable its daily job first on macOS, then uninstall:

```bash
agent-sync schedule uninstall --yes
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/alexanderattar/agent-sync/releases/latest/download/agent-sync-installer.sh | sh -s -- --uninstall
```

Uninstall removes the binary, not your synced files, configuration, backups,
logs, or chat exports. Private data under `~/.agent-sync` remains on disk.

## Build from source

With Rust and Cargo installed:

```bash
git clone https://github.com/alexanderattar/agent-sync.git
cd agent-sync
cargo build --release
mkdir -p ~/.local/bin
install -m 755 target/release/agent-sync ~/.local/bin/agent-sync
~/.local/bin/agent-sync
```

The source build includes Amp support. Install it at a stable path before
enabling a schedule.

## Advanced reference

Normal setup and daily use are covered above. For less common workflows:

- [Upgrade or uninstall](docs/reference.md#upgrade-or-uninstall)
- [Recovery and backups](docs/reference.md#recovery)
- [Supported configuration and Amp details](docs/reference.md#what-it-manages)
- [Manual setup, packs, and custom paths](docs/reference.md#managed-workflow)
- [Build from source](docs/reference.md#build-from-source) and [development checks](docs/reference.md#development)

## License

MIT

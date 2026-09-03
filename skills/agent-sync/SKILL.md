---
name: agent-sync
description: Set up, run, and diagnose agent-sync for Codex, Claude Code, and Cursor. Use when the user asks to sync agent configuration, keep agents aligned, check sync health or drift, repair agent-sync, or configure Cursor chat indexing.
---

# Agent Sync

Use the installed `agent-sync` command. Resolve it with `command -v agent-sync` first. If that fails and `~/.local/bin/agent-sync` is executable, use that absolute path for every command. Do not edit agent configuration directly when agent-sync supports the operation. The user can also open the interactive setup and status UI with the resolved executable.

- Start with `agent-sync status --format json`. If `configured` is `false`, route the request to setup. Read `cursor_history` as `disabled`, `export-only`, or `qmd`.
- For setup or a policy change, run `agent-sync setup` with only the requested options. Existing options that are not specified are preserved. Review the printed policy. Run the same command with `--yes` only when the user requested setup or that policy change.
- To sync agents, run `agent-sync sync` first. This previews additions, managed updates, and preserved target-owned conflicts. A `Skip` is intentional preservation and does not block unrelated additions. Use `agent-sync sync --yes` only when the user requested the write and the preview has no unexpected plain `Update`. Then run `agent-sync status`.
- To repair Cursor chat indexing, inspect `cursor_history` in the JSON status. If it is `qmd`, preview and apply sync. If it is `disabled` or `export-only`, preview `agent-sync setup --refresh-qmd`, save that policy with `--yes` when the user requested indexing, and then preview and apply sync. Finish with `agent-sync doctor`.
- For health, drift, or maintenance checks, run `agent-sync status`. Run `agent-sync doctor` when a check fails, is stale, or is unclear. Doctor exits nonzero when it finds a problem; use its output as the diagnosis.
- To manage the background schedule on macOS, use `agent-sync schedule status`, preview with `agent-sync schedule install` or `agent-sync schedule uninstall`, and add `--yes` only after the user approves that exact change. Do not create another scheduler job. The managed job uses `agent-sync sync --yes --automation`.
- For inventory only, run `agent-sync discover`.
- For recovery, start with `agent-sync status --format json` and `agent-sync doctor`, then inspect the recorded backup path. Show a path-specific restore plan and get explicit approval before copying or removing files. Do not restore an entire backup tree blindly.
- Before a repair, run the matching preview. Preserve conflicting skills, Claude Code- and Cursor-owned configuration, symlinks, and unmanaged hooks.
- Never widen MCP selection, enable every MCP server, or add a schedule without explicit approval.
- Never ask the user to paste secrets. Do not copy authentication sessions, plugin state, browser state, or chat databases. Cursor history is an explicit opt-in that creates redacted private copies under `~/.agent-sync/history/cursor`; remind the user that redaction is not a guarantee when they enable it.
- Treat failed verification and final drift as unhealthy. When Cursor history is enabled, also treat a broken hook as unhealthy. When `cursor_history` is `qmd`, treat missing coverage in `qmd://agent-sync-cursor/`, missing Cursor exports, or agent-sync's pending-export markers as unhealthy. Unrelated global QMD embedding work is a warning, not an agent-sync failure.
- If neither executable path is available, stop and explain that the binary must be installed. Do not build or download it unless asked.

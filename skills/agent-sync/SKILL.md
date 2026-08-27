---
name: agent-sync
description: Set up, run, and diagnose agent-sync for Codex, Claude Code, and Cursor. Use when the user asks to sync agent configuration, keep agents aligned, check sync health or drift, repair agent-sync, or configure Cursor chat indexing.
---

# Agent Sync

Use the installed `agent-sync` command. Do not edit agent configuration directly when agent-sync supports the operation.

- Start with `agent-sync status`. If no managed config exists, route the request to setup.
- For setup or a policy change, run `agent-sync setup` with only the requested options. Existing options that are not specified are preserved. Review the printed policy. Run the same command with `--yes` only when the user requested setup or that policy change.
- To sync agents, run `agent-sync sync` first. This previews every addition and replacement. Use `agent-sync sync --yes` only when the user requested the write and the preview has no unresolved conflict. Then run `agent-sync status`.
- To repair Cursor chat indexing, inspect the `Cursor history` line in `agent-sync status`. If it says `enabled with QMD refresh`, preview and apply sync. If it says `disabled` or `enabled without QMD refresh`, preview `agent-sync setup --refresh-qmd`, save that policy with `--yes` when the user requested the repair, and then preview and apply sync. Finish with `agent-sync doctor`.
- For health, drift, or maintenance checks, run `agent-sync status`. Run `agent-sync doctor` when a check fails, is stale, or is unclear. Doctor exits nonzero when it finds a problem; use its output as the diagnosis.
- For an approved external schedule, use `agent-sync sync --yes --automation` as the complete scheduled job. This command does not create the schedule. Do not recreate the low-level pack workflow in the scheduler.
- For inventory only, run `agent-sync discover`.
- Before a repair, run the matching preview. Preserve conflicting skills, Cursor-owned configuration, symlinks, and unmanaged hooks.
- Never widen MCP selection, enable every MCP server, or add a schedule without explicit approval.
- Never copy secrets, authentication sessions, plugin state, browser state, or chat databases.
- Treat failed verification and final drift as unhealthy. When Cursor history is enabled, also treat a broken hook as unhealthy. When QMD refresh is enabled, treat missing collection coverage or pending embeddings as unhealthy.
- If `agent-sync` is unavailable, stop and explain that the binary must be installed. Do not build or download it unless asked.

use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::{
    adapters::AgentPaths,
    fsx::{ensure_dir, read_to_string_if_exists, replace_file_with_backup, write_atomic},
};

const CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS: u64 = 300;
const MANAGED_HOOK_COMMAND_PREFIX: &str = "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 ";
const MANAGED_HOOK_COMMAND_SUFFIX: &str = " cursor-history export";
const MANAGED_HOOK_SKIP_QMD_COMMAND_SUFFIX: &str = " cursor-history export --skip-qmd";
const QMD_REFRESH_LOCK_STALE_AFTER: Duration = Duration::from_secs(10 * 60);
const QMD_REFRESH_LOCK_WAIT: Duration = Duration::from_millis(500);
const CURSOR_EXPORT_LOCK_MAX_WAITS: usize = 240;
const QMD_FORCE_LOCK_MAX_WAITS: usize = 1_200;
const QMD_PENDING_MARKER_PREFIX: &str = "agent-sync-qmd-";
const QMD_PENDING_MARKER_SUFFIX: &str = ".pending";
const QMD_PENDING_MARKER_MAGIC: &str = "agent-sync-qmd-pending-v1";
static QMD_PENDING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorHistoryInstallReport {
    pub changed: bool,
    pub dry_run: bool,
    pub hooks_path: PathBuf,
    pub backup: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorHistoryRemoveReport {
    pub changed: bool,
    pub dry_run: bool,
    pub hooks_path: PathBuf,
    pub backup: Option<PathBuf>,
}

impl CursorHistoryRemoveReport {
    pub fn to_text(&self) -> String {
        if self.changed && self.dry_run {
            format!(
                "Dry run. Remove managed Cursor history hook -> {}\n",
                self.hooks_path.display()
            )
        } else if self.changed {
            format!(
                "Removed managed Cursor history hook -> {}\n",
                self.hooks_path.display()
            )
        } else {
            format!(
                "No managed Cursor history hook -> {}\n",
                self.hooks_path.display()
            )
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QmdHealth {
    pub sessions_path: PathBuf,
    pub sessions_pattern: String,
    pub sessions_included: bool,
    pub pending_embeddings: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorHistoryCoverage {
    pub transcripts: usize,
    pub expected_exports: Vec<PathBuf>,
    pub missing: Vec<PathBuf>,
    pub stale: Vec<PathBuf>,
    pub unreadable: Vec<PathBuf>,
}

impl CursorHistoryCoverage {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty() && self.stale.is_empty() && self.unreadable.is_empty()
    }
}

impl CursorHistoryInstallReport {
    pub fn to_text(&self) -> String {
        if self.changed && self.dry_run {
            format!(
                "Dry run. Add Cursor history hook -> {}\n",
                self.hooks_path.display()
            )
        } else if self.changed {
            match &self.backup {
                Some(backup) => format!(
                    "Added Cursor history hook -> {}\nBackup: {}\n",
                    self.hooks_path.display(),
                    backup.display()
                ),
                None => format!(
                    "Added Cursor history hook -> {}\n",
                    self.hooks_path.display()
                ),
            }
        } else {
            format!(
                "Unchanged Cursor history hook -> {}\n",
                self.hooks_path.display()
            )
        }
    }
}

pub fn install_cursor_history_hook(
    paths: &AgentPaths,
    executable: &Path,
    dry_run: bool,
) -> Result<CursorHistoryInstallReport> {
    install_cursor_history_hook_with_refresh(paths, executable, dry_run, true)
}

pub fn install_cursor_history_hook_with_refresh(
    paths: &AgentPaths,
    executable: &Path,
    dry_run: bool,
    refresh_qmd: bool,
) -> Result<CursorHistoryInstallReport> {
    let hooks_path = paths.cursor_home.join("hooks.json");
    ensure_cursor_hooks_write_safe(&hooks_path)?;
    let suffix = if refresh_qmd {
        MANAGED_HOOK_COMMAND_SUFFIX
    } else {
        MANAGED_HOOK_SKIP_QMD_COMMAND_SUFFIX
    };
    let command = format!(
        "{MANAGED_HOOK_COMMAND_PREFIX}{}{suffix}",
        shell_quote(&executable.to_string_lossy())
    );
    let (_, changed) = render_cursor_hooks(&hooks_path, &command)?;
    if dry_run || !changed {
        return Ok(CursorHistoryInstallReport {
            changed,
            dry_run,
            hooks_path,
            backup: None,
        });
    }

    // Re-render from the latest Cursor-owned file immediately before writing so
    // hooks added after the preview are included instead of overwritten.
    let (content, changed) = render_cursor_hooks(&hooks_path, &command)?;
    if !changed {
        return Ok(CursorHistoryInstallReport {
            changed: false,
            dry_run: false,
            hooks_path,
            backup: None,
        });
    }

    let backup_root = paths
        .home
        .join(".agent-sync")
        .join("backups")
        .join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    let backup = replace_file_with_backup(&backup_root, &paths.cursor_home, &hooks_path, &content)?;
    Ok(CursorHistoryInstallReport {
        changed,
        dry_run,
        hooks_path,
        backup,
    })
}

pub fn remove_cursor_history_hook(
    paths: &AgentPaths,
    dry_run: bool,
) -> Result<CursorHistoryRemoveReport> {
    let hooks_path = paths.cursor_home.join("hooks.json");
    ensure_cursor_hooks_write_safe(&hooks_path)?;
    let (_, changed) = render_cursor_hooks_without_managed(&hooks_path)?;
    if dry_run || !changed {
        return Ok(CursorHistoryRemoveReport {
            changed,
            dry_run,
            hooks_path,
            backup: None,
        });
    }

    let (content, changed) = render_cursor_hooks_without_managed(&hooks_path)?;
    if !changed {
        return Ok(CursorHistoryRemoveReport {
            changed: false,
            dry_run: false,
            hooks_path,
            backup: None,
        });
    }

    let backup_root = paths
        .home
        .join(".agent-sync")
        .join("backups")
        .join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    let backup = replace_file_with_backup(&backup_root, &paths.cursor_home, &hooks_path, &content)?;
    Ok(CursorHistoryRemoveReport {
        changed,
        dry_run: false,
        hooks_path,
        backup,
    })
}

pub fn export_cursor_history_from_stdin(
    paths: &AgentPaths,
    output_dir: Option<PathBuf>,
    refresh_qmd: bool,
) -> Result<Option<PathBuf>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let hook: Value = serde_json::from_str(input.trim()).context("parse Cursor hook input")?;
    export_cursor_history(paths, &hook, output_dir, refresh_qmd)
}

pub fn export_cursor_history(
    paths: &AgentPaths,
    hook: &Value,
    output_dir: Option<PathBuf>,
    refresh_qmd: bool,
) -> Result<Option<PathBuf>> {
    let export_lock = acquire_cursor_history_export_lock(paths)?;
    let Some(transcript_path) = hook.get("transcript_path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let transcript_path = PathBuf::from(transcript_path);
    let transcript_path = transcript_path
        .canonicalize()
        .with_context(|| format!("resolve {}", transcript_path.display()))?;
    let cursor_projects = paths.cursor_home.join("projects");
    let cursor_projects = cursor_projects
        .canonicalize()
        .with_context(|| format!("resolve {}", cursor_projects.display()))?;
    if !transcript_path.starts_with(&cursor_projects) {
        anyhow::bail!(
            "Cursor transcript {} is outside {}",
            transcript_path.display(),
            cursor_projects.display()
        );
    }

    let raw = fs::read_to_string(&transcript_path)
        .with_context(|| format!("read {}", transcript_path.display()))?;
    let turns = cursor_turns(&raw).with_context(|| {
        format!(
            "Cursor transcript {} has an unsupported entry; Cursor may have changed its transcript format",
            transcript_path.display()
        )
    })?;
    if turns.is_empty() {
        if raw.trim().is_empty() {
            return Ok(None);
        }
        anyhow::bail!(
            "Cursor transcript {} contains no supported user or assistant turns; Cursor may have changed its transcript format",
            transcript_path.display()
        );
    }

    let derived_conversation_id = fallback_conversation_id(&transcript_path, &cursor_projects)
        .context("Cursor transcript path has no conversation id")?;
    let conversation_id = match hook.get("conversation_id").and_then(Value::as_str) {
        Some(supplied) if supplied == derived_conversation_id => derived_conversation_id,
        Some(supplied) => anyhow::bail!(
            "Cursor hook conversation id {supplied:?} does not match transcript id {derived_conversation_id:?}"
        ),
        None => derived_conversation_id,
    };
    let modified = fs::metadata(&transcript_path)?.modified()?;
    let timestamp: DateTime<Utc> = modified.into();
    let output_dir = output_dir.unwrap_or_else(|| {
        paths
            .home
            .join("Documents")
            .join("Obsidian")
            .join("sessions")
    });
    fs::create_dir_all(&output_dir).with_context(|| format!("create {}", output_dir.display()))?;
    let output = output_dir.join(format!("cursor-{}.md", safe_filename(&conversation_id)));
    let existing = read_existing_cursor_export(&output, &conversation_id)?;
    let workspace_roots = hook
        .get("workspace_roots")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .or_else(|| {
            existing
                .as_deref()
                .and_then(|content| existing_json_frontmatter(content, "workspace_roots"))
                .and_then(|value| {
                    value.as_array().map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                    })
                })
        })
        .unwrap_or_default();
    let model = hook
        .get("model")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            existing
                .as_deref()
                .and_then(|content| existing_json_frontmatter(content, "model"))
                .and_then(|value| value.as_str().map(ToString::to_string))
        })
        .unwrap_or_default();
    let content = render_markdown(
        &conversation_id,
        timestamp,
        &model,
        &workspace_roots,
        &turns,
    )?;

    let changed = existing.as_deref() != Some(content.as_str());
    if changed {
        write_atomic(&output, content.as_bytes())?;
    }
    if refresh_qmd {
        write_qmd_pending_marker(paths, &conversation_id)?;
    }
    drop(export_lock);
    if refresh_qmd {
        refresh_qmd_index(paths, false)?;
    }
    Ok(Some(output))
}

/// Exports every readable Cursor agent transcript so a scheduled sync can
/// recover sessions missed by the stop hook.
pub fn sweep_cursor_history(paths: &AgentPaths, mark_qmd_pending: bool) -> Result<usize> {
    let mut exported = 0;
    for transcript in cursor_transcript_paths(paths)? {
        let hook = json!({"transcript_path": transcript});
        if let Some(output) = export_cursor_history(paths, &hook, None, false)? {
            exported += 1;
            if mark_qmd_pending {
                let marker = output
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("cursor-history");
                write_qmd_pending_marker(paths, marker)?;
            }
        }
    }
    Ok(exported)
}

pub fn cursor_history_coverage(paths: &AgentPaths) -> Result<CursorHistoryCoverage> {
    let projects = paths.cursor_home.join("projects");
    if !projects.exists() {
        return Ok(CursorHistoryCoverage {
            transcripts: 0,
            expected_exports: Vec::new(),
            missing: Vec::new(),
            stale: Vec::new(),
            unreadable: Vec::new(),
        });
    }
    let canonical_projects = projects
        .canonicalize()
        .with_context(|| format!("resolve {}", projects.display()))?;
    let output_dir = paths.home.join("Documents/Obsidian/sessions");
    let mut expected_exports = Vec::new();
    let mut missing = Vec::new();
    let mut stale = Vec::new();
    let mut unreadable = Vec::new();
    let mut transcripts = 0;
    for transcript in cursor_transcript_paths(paths)? {
        let transcript = transcript
            .canonicalize()
            .with_context(|| format!("resolve {}", transcript.display()))?;
        let raw = fs::read_to_string(&transcript)
            .with_context(|| format!("read {}", transcript.display()))?;
        if raw.trim().is_empty() {
            continue;
        }
        transcripts += 1;
        let turns = match cursor_turns(&raw) {
            Ok(turns) if !turns.is_empty() => turns,
            Ok(_) | Err(_) => {
                unreadable.push(transcript);
                continue;
            }
        };
        let conversation_id = fallback_conversation_id(&transcript, &canonical_projects)
            .context("Cursor transcript has no conversation id")?;
        let output = output_dir.join(format!("cursor-{}.md", safe_filename(&conversation_id)));
        expected_exports.push(output.clone());
        match fs::symlink_metadata(&output) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                stale.push(output);
                continue;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                missing.push(output);
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", output.display()));
            }
        }
        let content =
            fs::read_to_string(&output).with_context(|| format!("read {}", output.display()))?;
        let workspace_roots = existing_json_frontmatter(&content, "workspace_roots")
            .and_then(|value| {
                value.as_array().map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        let model = existing_json_frontmatter(&content, "model")
            .and_then(|value| value.as_str().map(ToString::to_string))
            .unwrap_or_default();
        let timestamp: DateTime<Utc> = fs::metadata(&transcript)?.modified()?.into();
        let expected = render_markdown(
            &conversation_id,
            timestamp,
            &model,
            &workspace_roots,
            &turns,
        )?;
        if content != expected {
            stale.push(output);
        }
    }
    Ok(CursorHistoryCoverage {
        transcripts,
        expected_exports,
        missing,
        stale,
        unreadable,
    })
}

fn cursor_transcript_paths(paths: &AgentPaths) -> Result<Vec<PathBuf>> {
    let projects = paths.cursor_home.join("projects");
    if !projects.exists() {
        return Ok(Vec::new());
    }
    let mut transcripts = Vec::new();
    for entry in WalkDir::new(&projects).follow_links(false) {
        let entry =
            entry.with_context(|| format!("scan Cursor projects at {}", projects.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        let is_transcript = path.strip_prefix(&projects).is_ok_and(|relative| {
            relative
                .components()
                .any(|component| component.as_os_str() == std::ffi::OsStr::new("agent-transcripts"))
        }) && path.extension().and_then(|extension| extension.to_str())
            == Some("jsonl");
        if is_transcript {
            transcripts.push(path);
        }
    }
    transcripts.sort();
    Ok(transcripts)
}

fn render_cursor_hooks(path: &Path, command: &str) -> Result<(Vec<u8>, bool)> {
    let mut root: Value = match read_to_string_if_exists(path)? {
        Some(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parse Cursor hooks file {}", path.display()))?,
        None => json!({}),
    };
    let object = root
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let version_changed = match object.get("version") {
        Some(version) if version.as_u64() == Some(1) => false,
        Some(version) => anyhow::bail!(
            "{}.version must be the numeric value 1, found {version}; refusing to rewrite Cursor hooks",
            path.display()
        ),
        None => {
            object.insert("version".to_string(), json!(1));
            true
        }
    };
    let hooks = object
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("{}.hooks must be a JSON object", path.display()))?;
    let stop = hooks
        .entry("stop".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .with_context(|| format!("{}.hooks.stop must be a JSON array", path.display()))?;
    let managed = stop
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_managed_hook(entry))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if managed.len() > 1 {
        anyhow::bail!(
            "{}.hooks.stop contains more than one agent-sync-managed Cursor history hook",
            path.display()
        );
    }
    if let Some(index) = managed.first().copied() {
        let entry = stop[index]
            .as_object_mut()
            .context("agent-sync-managed Cursor history hook must be a JSON object")?;
        let command_matches = entry.get("command").and_then(Value::as_str) == Some(command);
        let timeout_matches = entry.get("timeout").and_then(Value::as_u64)
            == Some(CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS);
        if command_matches && timeout_matches {
            return Ok((
                [serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat(),
                version_changed,
            ));
        }
        entry.insert("command".to_string(), json!(command));
        entry.insert(
            "timeout".to_string(),
            json!(CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS),
        );
        return Ok((
            [serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat(),
            true,
        ));
    }
    stop.push(json!({
        "command": command,
        "timeout": CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS
    }));
    Ok((
        [serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat(),
        true,
    ))
}

fn render_cursor_hooks_without_managed(path: &Path) -> Result<(Vec<u8>, bool)> {
    let Some(raw) = read_to_string_if_exists(path)? else {
        return Ok((Vec::new(), false));
    };
    let mut root: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse Cursor hooks file {}", path.display()))?;
    let Some(stop) = root
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .and_then(|hooks| hooks.get_mut("stop"))
    else {
        return Ok((raw.into_bytes(), false));
    };
    let stop = stop
        .as_array_mut()
        .with_context(|| format!("{}.hooks.stop must be a JSON array", path.display()))?;
    let managed = stop.iter().filter(|entry| is_managed_hook(entry)).count();
    if managed > 1 {
        anyhow::bail!(
            "{}.hooks.stop contains more than one agent-sync-managed Cursor history hook",
            path.display()
        );
    }
    if managed == 0 {
        return Ok((raw.into_bytes(), false));
    }
    stop.retain(|entry| !is_managed_hook(entry));
    Ok((
        [serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat(),
        true,
    ))
}

fn ensure_cursor_hooks_write_safe(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
            "refusing to rewrite symlinked Cursor hooks file {}",
            path.display()
        ),
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!(
                "Cursor hooks path is not a regular file: {}",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn cursor_turns(raw: &str) -> Result<Vec<(String, String)>> {
    let mut turns = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line)
            .with_context(|| format!("parse Cursor transcript line {}", index + 1))?;
        let object = entry
            .as_object()
            .with_context(|| format!("Cursor transcript line {} is not an object", index + 1))?;
        let Some(role_value) = object.get("role") else {
            if object.get("type").and_then(Value::as_str) == Some("turn_ended") {
                continue;
            }
            anyhow::bail!(
                "Cursor transcript line {} is neither a message nor a turn_ended record",
                index + 1
            );
        };
        let role = role_value.as_str().with_context(|| {
            format!("Cursor transcript line {} has a non-string role", index + 1)
        })?;
        if !matches!(role, "user" | "assistant") {
            anyhow::bail!(
                "Cursor transcript line {} has unsupported role {role:?}",
                index + 1
            );
        }
        let items = object
            .get("message")
            .and_then(Value::as_object)
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .with_context(|| {
                format!(
                    "Cursor transcript line {} has no message.content array",
                    index + 1
                )
            })?;
        let mut text = Vec::new();
        for item in items {
            let item = item.as_object().with_context(|| {
                format!(
                    "Cursor transcript line {} has a non-object content item",
                    index + 1
                )
            })?;
            let item_type = item.get("type").and_then(Value::as_str).with_context(|| {
                format!(
                    "Cursor transcript line {} has a content item without a string type",
                    index + 1
                )
            })?;
            match item_type {
                "text" => {
                    text.push(item.get("text").and_then(Value::as_str).with_context(|| {
                        format!(
                            "Cursor transcript line {} has a text item without string text",
                            index + 1
                        )
                    })?)
                }
                "tool_use" => {}
                _ => anyhow::bail!(
                    "Cursor transcript line {} has unsupported content type {item_type:?}",
                    index + 1
                ),
            }
        }
        let content = text.join("\n");
        if !content.trim().is_empty() {
            turns.push((role.to_string(), content));
        }
    }
    Ok(turns)
}

fn existing_json_frontmatter(content: &str, key: &str) -> Option<Value> {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return None;
    }
    let prefix = format!("{key}:");
    for line in lines {
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix(&prefix) {
            return serde_json::from_str(value.trim()).ok();
        }
    }
    None
}

fn read_existing_cursor_export(path: &Path, conversation_id: &str) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "refusing to replace non-regular Cursor history export {}",
            path.display()
        );
    }
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let source_matches = content
        .lines()
        .skip(1)
        .take_while(|line| *line != "---")
        .any(|line| line == "source: cursor");
    let conversation_matches = existing_json_frontmatter(&content, "conversation_id")
        .and_then(|value| value.as_str().map(ToString::to_string))
        .as_deref()
        == Some(conversation_id);
    if !source_matches || !conversation_matches {
        anyhow::bail!(
            "refusing to replace target-owned file at Cursor history export path {}",
            path.display()
        );
    }
    Ok(Some(content))
}

fn fallback_conversation_id(transcript_path: &Path, cursor_projects: &Path) -> Option<String> {
    let stem = transcript_path.file_stem()?.to_str()?;
    let parent = transcript_path.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("subagents") {
        let conversation = parent.parent()?.file_name()?.to_str()?;
        Some(format!("{conversation}-subagent-{stem}"))
    } else {
        transcript_path
            .strip_prefix(cursor_projects)
            .ok()
            .map(|_| stem.to_string())
    }
}

fn render_markdown(
    conversation_id: &str,
    timestamp: DateTime<Utc>,
    model: &str,
    workspace_roots: &[String],
    turns: &[(String, String)],
) -> Result<String> {
    let mut out = String::new();
    out.push_str("---\nsource: cursor\n");
    out.push_str(&format!(
        "conversation_id: {}\n",
        serde_json::to_string(conversation_id)?
    ));
    out.push_str(&format!("date: {}\n", timestamp.to_rfc3339()));
    out.push_str(&format!("model: {}\n", serde_json::to_string(model)?));
    out.push_str(&format!(
        "workspace_roots: {}\n---\n\n",
        serde_json::to_string(workspace_roots)?
    ));
    out.push_str(&format!("# Cursor session `{conversation_id}`\n\n"));
    for (role, text) in turns {
        let heading = if role == "user" { "User" } else { "Cursor" };
        out.push_str(&format!("## {heading}\n\n{text}\n\n"));
    }
    Ok(out)
}

fn is_managed_hook(entry: &Value) -> bool {
    let Some(command) = entry
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| command.strip_prefix(MANAGED_HOOK_COMMAND_PREFIX))
    else {
        return false;
    };
    [
        MANAGED_HOOK_COMMAND_SUFFIX,
        MANAGED_HOOK_SKIP_QMD_COMMAND_SUFFIX,
    ]
    .iter()
    .any(|suffix| {
        command
            .strip_suffix(suffix)
            .is_some_and(|executable| !executable.is_empty())
    })
}

pub fn refresh_qmd_index(paths: &AgentPaths, force: bool) -> Result<bool> {
    let mut waits = 0;
    let _lock = loop {
        if let Some(lock) = acquire_qmd_refresh_lock(paths)? {
            break lock;
        }
        if !force {
            return Ok(false);
        }
        if waits >= QMD_FORCE_LOCK_MAX_WAITS {
            anyhow::bail!("timed out waiting for another QMD refresh to finish");
        }
        waits += 1;
        std::thread::sleep(QMD_REFRESH_LOCK_WAIT);
    };
    let refresh_started = SystemTime::now();

    let qmd = qmd_executable(paths).context("QMD executable was not found in a standard path")?;
    run_qmd_command(&qmd, "update")?;
    run_qmd_command(&qmd, "embed")?;

    let health = qmd_health(paths)?;
    let expected_sessions = paths.home.join("Documents/Obsidian/sessions");
    let actual_sessions =
        fs::canonicalize(&health.sessions_path).unwrap_or_else(|_| health.sessions_path.clone());
    let expected_sessions = fs::canonicalize(&expected_sessions).unwrap_or(expected_sessions);
    if actual_sessions != expected_sessions {
        anyhow::bail!(
            "QMD sessions collection points to {}, expected {}",
            actual_sessions.display(),
            expected_sessions.display()
        );
    }
    if health.pending_embeddings > 0 {
        anyhow::bail!(
            "QMD still has {} pending embedding(s) after refresh",
            health.pending_embeddings
        );
    }

    let coverage = cursor_history_coverage(paths)?;
    if !coverage.is_complete() {
        anyhow::bail!(
            "Cursor history coverage is incomplete after refresh: {} missing, {} stale, {} unreadable",
            coverage.missing.len(),
            coverage.stale.len(),
            coverage.unreadable.len()
        );
    }
    for export in &coverage.expected_exports {
        if !qmd_export_is_indexed(paths, export)? {
            anyhow::bail!(
                "Cursor history export is not retrievable from QMD: {}",
                export.display()
            );
        }
    }

    write_qmd_refresh_state(paths, SystemTime::now())?;
    clear_qmd_pending_markers(paths, refresh_started)?;
    Ok(true)
}

pub fn qmd_executable(paths: &AgentPaths) -> Option<PathBuf> {
    [
        paths.home.join(".local/bin/qmd"),
        PathBuf::from("/usr/local/bin/qmd"),
        PathBuf::from("/opt/homebrew/bin/qmd"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

pub fn qmd_refresh_last_success(paths: &AgentPaths) -> Result<Option<DateTime<Utc>>> {
    let state_path = qmd_refresh_state_dir(paths).join("qmd-refresh-state.json");
    let Some(raw) = read_to_string_if_exists(&state_path)? else {
        return Ok(None);
    };
    let state: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse QMD refresh state {}", state_path.display()))?;
    let Some(millis) = state.get("lastSuccessUnixMillis").and_then(Value::as_u64) else {
        return Ok(None);
    };
    let seconds: i64 = (millis / 1_000)
        .try_into()
        .context("QMD refresh timestamp does not fit in i64")?;
    Ok(DateTime::<Utc>::from_timestamp(
        seconds,
        ((millis % 1_000) * 1_000_000) as u32,
    ))
}

pub fn qmd_pending_exports(paths: &AgentPaths) -> Result<usize> {
    let Some(pending) = checked_qmd_pending_dir(paths, false)? else {
        return Ok(0);
    };
    let mut count = 0;
    for entry in fs::read_dir(&pending).with_context(|| format!("read {}", pending.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() && is_managed_qmd_pending_marker(&entry.path())? {
            count += 1;
        }
    }
    Ok(count)
}

pub fn qmd_health(paths: &AgentPaths) -> Result<QmdHealth> {
    let qmd = qmd_executable(paths).context("QMD executable was not found in a standard path")?;
    let collection = Command::new(&qmd)
        .args(["collection", "show", "sessions"])
        .output()
        .context("run qmd collection show sessions")?;
    if !collection.status.success() {
        anyhow::bail!(
            "qmd sessions collection check failed with {}: {}",
            collection.status,
            String::from_utf8_lossy(&collection.stderr).trim()
        );
    }
    let collection_stdout =
        String::from_utf8(collection.stdout).context("read QMD collection output")?;
    let sessions_path = collection_stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Path:"))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .context("qmd sessions collection output has no path")?;
    let sessions_pattern = collection_stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Pattern:"))
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(ToString::to_string)
        .context("qmd sessions collection output has no pattern")?;
    let sessions_included = collection_stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Include:"))
        .map(str::trim)
        .map(|include| include.to_ascii_lowercase().starts_with("yes"))
        .context("qmd sessions collection output has no include state")?;
    if !sessions_included {
        anyhow::bail!("QMD sessions collection is excluded from global search");
    }
    if !qmd_pattern_covers_cursor_exports(&sessions_pattern) {
        anyhow::bail!(
            "QMD sessions collection pattern {sessions_pattern:?} does not cover cursor-*.md"
        );
    }

    let status = Command::new(&qmd)
        .arg("status")
        .output()
        .context("run qmd status")?;
    if !status.status.success() {
        anyhow::bail!(
            "qmd status failed with {}: {}",
            status.status,
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    let status_stdout = String::from_utf8(status.stdout).context("read QMD status output")?;
    let pending_embeddings = parse_qmd_pending_embeddings(&status_stdout)?;

    Ok(QmdHealth {
        sessions_path,
        sessions_pattern,
        sessions_included,
        pending_embeddings,
    })
}

fn parse_qmd_pending_embeddings(status: &str) -> Result<usize> {
    if let Some(value) = status
        .lines()
        .find_map(|line| line.trim().strip_prefix("Pending:"))
    {
        return value
            .split_whitespace()
            .next()
            .context("qmd status pending line has no count")?
            .parse::<usize>()
            .context("qmd status pending count is not a number");
    }

    let has_documents = status.lines().any(|line| line.trim() == "Documents");
    let has_vectors = status
        .lines()
        .any(|line| line.trim().starts_with("Vectors:"));
    if has_documents && has_vectors {
        // Current QMD omits the Pending row when the count is zero.
        return Ok(0);
    }
    anyhow::bail!("qmd status output has no recognizable embedding state")
}

fn qmd_pattern_covers_cursor_exports(patterns: &str) -> bool {
    const PROBES: [&str; 3] = [
        "cursor-a.md",
        "cursor-agent_sync-123.md",
        "cursor-agent-sync-coverage-probe.md",
    ];

    patterns
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| {
            let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
            PROBES
                .iter()
                .all(|candidate| glob_matches(pattern.as_bytes(), candidate.as_bytes()))
        })
}

fn glob_matches(pattern: &[u8], candidate: &[u8]) -> bool {
    fn matches_from(
        pattern: &[u8],
        candidate: &[u8],
        pattern_index: usize,
        candidate_index: usize,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[pattern_index][candidate_index] {
            return result;
        }
        let result = if pattern_index == pattern.len() {
            candidate_index == candidate.len()
        } else if pattern[pattern_index] == b'*' && pattern.get(pattern_index + 1) == Some(&b'*') {
            let mut next = pattern_index + 2;
            while pattern.get(next) == Some(&b'*') {
                next += 1;
            }
            if pattern.get(next) == Some(&b'/') {
                matches_from(pattern, candidate, next + 1, candidate_index, memo)
                    || (candidate_index < candidate.len()
                        && matches_from(
                            pattern,
                            candidate,
                            pattern_index,
                            candidate_index + 1,
                            memo,
                        ))
            } else {
                matches_from(pattern, candidate, next, candidate_index, memo)
                    || (candidate_index < candidate.len()
                        && matches_from(
                            pattern,
                            candidate,
                            pattern_index,
                            candidate_index + 1,
                            memo,
                        ))
            }
        } else if pattern[pattern_index] == b'*' {
            matches_from(pattern, candidate, pattern_index + 1, candidate_index, memo)
                || (candidate_index < candidate.len()
                    && candidate[candidate_index] != b'/'
                    && matches_from(pattern, candidate, pattern_index, candidate_index + 1, memo))
        } else if candidate_index < candidate.len()
            && (pattern[pattern_index] == b'?'
                || pattern[pattern_index] == candidate[candidate_index])
        {
            matches_from(
                pattern,
                candidate,
                pattern_index + 1,
                candidate_index + 1,
                memo,
            )
        } else {
            false
        };
        memo[pattern_index][candidate_index] = Some(result);
        result
    }

    let mut memo = vec![vec![None; candidate.len() + 1]; pattern.len() + 1];
    matches_from(pattern, candidate, 0, 0, &mut memo)
}

pub fn qmd_export_is_indexed(paths: &AgentPaths, export: &Path) -> Result<bool> {
    let qmd = qmd_executable(paths).context("QMD executable was not found in a standard path")?;
    let name = export
        .file_name()
        .and_then(|name| name.to_str())
        .context("Cursor history export has no UTF-8 file name")?;
    let virtual_path = format!("qmd://sessions/{name}");
    let max_bytes = fs::metadata(export)
        .with_context(|| format!("inspect Cursor history export {}", export.display()))?
        .len()
        .saturating_add(4_096);
    // The trailing comma intentionally selects QMD's exact-path list mode.
    // Without it, multi-get treats a single path as a glob and can suffix-match
    // another document with the same basename.
    let output = Command::new(qmd)
        .args([
            "multi-get",
            &format!("{virtual_path},"),
            "--json",
            "--max-bytes",
            &max_bytes.to_string(),
        ])
        .output()
        .with_context(|| format!("check QMD index for {}", export.display()))?;
    if !output.status.success() {
        return Ok(false);
    }
    let indexed: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse indexed QMD export {}", export.display()))?;
    let current = fs::read_to_string(export)
        .with_context(|| format!("read Cursor history export {}", export.display()))?;
    let Some(documents) = indexed.as_array() else {
        anyhow::bail!("qmd multi-get did not return a JSON array");
    };
    if documents.len() != 1 {
        return Ok(false);
    }
    let document = &documents[0];
    let exact_path = document.get("file").and_then(Value::as_str) == Some(&virtual_path);
    let body = document.get("body").and_then(Value::as_str);
    Ok(exact_path
        && body.is_some_and(|body| {
            body.trim_end_matches(['\r', '\n']) == current.trim_end_matches(['\r', '\n'])
        }))
}

fn run_qmd_command(qmd: &Path, subcommand: &str) -> Result<()> {
    let output = Command::new(qmd)
        .arg(subcommand)
        .stdout(Stdio::null())
        .output()
        .with_context(|| format!("run qmd {subcommand}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "qmd {subcommand} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

struct QmdRefreshLock {
    path: PathBuf,
    snapshot: LockSnapshot,
}

impl Drop for QmdRefreshLock {
    fn drop(&mut self) {
        let _ = remove_lock_if_snapshot_matches(&self.path, &self.snapshot);
    }
}

struct CursorHistoryExportLock {
    path: PathBuf,
    snapshot: LockSnapshot,
}

impl Drop for CursorHistoryExportLock {
    fn drop(&mut self) {
        let _ = remove_lock_if_snapshot_matches(&self.path, &self.snapshot);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LockSnapshot {
    token: String,
    identity: LockFileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LockFileIdentity {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl LockFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

fn acquire_cursor_history_export_lock(paths: &AgentPaths) -> Result<CursorHistoryExportLock> {
    let state_dir = qmd_refresh_state_dir(paths);
    ensure_dir(&state_dir)?;
    let path = state_dir.join("cursor-history-export.lock");
    let mut waits = 0;
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let token = format!("{}:{}", std::process::id(), unix_millis(SystemTime::now())?);
                writeln!(file, "{token}")
                    .with_context(|| format!("write Cursor history lock {}", path.display()))?;
                file.sync_all()?;
                let snapshot = read_lock_snapshot(&path)?.with_context(|| {
                    format!(
                        "Cursor history lock changed while acquiring {}",
                        path.display()
                    )
                })?;
                return Ok(CursorHistoryExportLock { path, snapshot });
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if remove_stale_lock_if_unchanged(&path)? {
                    continue;
                }
                if waits >= CURSOR_EXPORT_LOCK_MAX_WAITS {
                    anyhow::bail!("timed out waiting for Cursor history export lock");
                }
                waits += 1;
                std::thread::sleep(QMD_REFRESH_LOCK_WAIT);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create Cursor history lock {}", path.display()));
            }
        }
    }
}

fn acquire_qmd_refresh_lock(paths: &AgentPaths) -> Result<Option<QmdRefreshLock>> {
    let state_dir = qmd_refresh_state_dir(paths);
    ensure_dir(&state_dir)?;
    let path = state_dir.join("qmd-refresh.lock");

    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let token = format!("{}:{}", std::process::id(), unix_millis(SystemTime::now())?);
                writeln!(file, "{token}")
                    .with_context(|| format!("write QMD refresh lock {}", path.display()))?;
                file.sync_all()?;
                let snapshot = read_lock_snapshot(&path)?.with_context(|| {
                    format!(
                        "QMD refresh lock changed while acquiring {}",
                        path.display()
                    )
                })?;
                return Ok(Some(QmdRefreshLock {
                    path: path.clone(),
                    snapshot,
                }));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if !remove_stale_lock_if_unchanged(&path)? {
                    return Ok(None);
                }
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create QMD refresh lock {}", path.display()));
            }
        }
    }
    Ok(None)
}

fn read_lock_snapshot(path: &Path) -> Result<Option<LockSnapshot>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("lock path is a symlink: {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!("lock path is not a regular file: {}", path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect lock path {}", path.display()));
        }
    }
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open lock file {}", path.display()));
        }
    };
    let file_metadata = file
        .metadata()
        .with_context(|| format!("inspect open lock file {}", path.display()))?;
    if !file_metadata.is_file() {
        anyhow::bail!("lock path is not a regular file: {}", path.display());
    }
    let mut token = String::new();
    file.read_to_string(&mut token)
        .with_context(|| format!("read lock file {}", path.display()))?;

    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect lock path {}", path.display()));
        }
    };
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        anyhow::bail!("lock path is not a regular file: {}", path.display());
    }
    let file_identity = LockFileIdentity::from_metadata(&file_metadata)?;
    let path_identity = LockFileIdentity::from_metadata(&path_metadata)?;
    if file_identity != path_identity {
        return Ok(None);
    }

    Ok(Some(LockSnapshot {
        token: token.trim().to_string(),
        identity: file_identity,
    }))
}

fn remove_stale_lock_if_unchanged(path: &Path) -> Result<bool> {
    let Some(snapshot) = read_lock_snapshot(path)? else {
        return Ok(true);
    };
    if lock_owner_is_active(&snapshot.token) || !lock_snapshot_is_stale(&snapshot)? {
        return Ok(false);
    }
    remove_lock_if_snapshot_matches(path, &snapshot)
}

fn remove_lock_if_snapshot_matches(path: &Path, expected: &LockSnapshot) -> Result<bool> {
    let Some(current) = read_lock_snapshot(path)? else {
        return Ok(true);
    };
    if &current != expected {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).with_context(|| format!("remove lock file {}", path.display())),
    }
}

fn lock_snapshot_is_stale(snapshot: &LockSnapshot) -> Result<bool> {
    let now = unix_millis(SystemTime::now())?;
    let token_age = snapshot
        .token
        .split_once(':')
        .and_then(|(_, timestamp)| timestamp.parse::<u64>().ok())
        .map(|timestamp| Duration::from_millis(now.saturating_sub(timestamp)));
    let age = token_age.or_else(|| snapshot.identity.modified.elapsed().ok());
    Ok(age.is_some_and(|age| age >= QMD_REFRESH_LOCK_STALE_AFTER))
}

#[cfg(unix)]
fn lock_owner_is_active(token: &str) -> bool {
    let Some(pid) = token
        .split(':')
        .next()
        .and_then(|value| value.trim().parse::<u32>().ok())
    else {
        return false;
    };
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn lock_owner_is_active(_token: &str) -> bool {
    false
}

fn write_qmd_refresh_state(paths: &AgentPaths, now: SystemTime) -> Result<()> {
    let state_path = qmd_refresh_state_dir(paths).join("qmd-refresh-state.json");
    let content = [
        serde_json::to_vec_pretty(&json!({
            "lastSuccessUnixMillis": unix_millis(now)?,
        }))?,
        b"\n".to_vec(),
    ]
    .concat();
    write_atomic(&state_path, &content)
}

fn write_qmd_pending_marker(paths: &AgentPaths, conversation_id: &str) -> Result<()> {
    let timestamp = unix_millis(SystemTime::now())?;
    let sequence = QMD_PENDING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pending =
        checked_qmd_pending_dir(paths, true)?.context("QMD pending directory was not created")?;
    let path = pending.join(format!(
        "{QMD_PENDING_MARKER_PREFIX}{}-{timestamp}-{}-{sequence}{QMD_PENDING_MARKER_SUFFIX}",
        safe_filename(conversation_id),
        std::process::id()
    ));
    write_atomic(
        &path,
        format!("{QMD_PENDING_MARKER_MAGIC}\n{timestamp}\n").as_bytes(),
    )
}

fn clear_qmd_pending_markers(paths: &AgentPaths, refresh_started: SystemTime) -> Result<()> {
    let Some(pending) = checked_qmd_pending_dir(paths, false)? else {
        return Ok(());
    };
    for entry in fs::read_dir(&pending).with_context(|| format!("read {}", pending.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || !is_managed_qmd_pending_marker(&entry.path())? {
            continue;
        }
        let path = entry.path();
        let Some(snapshot) = read_lock_snapshot(&path)? else {
            continue;
        };
        if snapshot.identity.modified <= refresh_started {
            remove_lock_if_snapshot_matches(&path, &snapshot)?;
        }
    }
    Ok(())
}

fn checked_qmd_pending_dir(paths: &AgentPaths, create: bool) -> Result<Option<PathBuf>> {
    let pending = qmd_pending_dir(paths);
    match fs::symlink_metadata(&pending) {
        Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
            "refusing to use symlinked QMD pending directory {}",
            pending.display()
        ),
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!("QMD pending path is not a directory: {}", pending.display())
        }
        Ok(_) => Ok(Some(pending)),
        Err(error) if error.kind() == ErrorKind::NotFound && create => {
            ensure_dir(&pending)?;
            let metadata = fs::symlink_metadata(&pending)
                .with_context(|| format!("inspect {}", pending.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!(
                    "QMD pending path became unsafe while creating it: {}",
                    pending.display()
                );
            }
            Ok(Some(pending))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", pending.display())),
    }
}

fn is_managed_qmd_pending_marker(path: &Path) -> Result<bool> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    if !name.starts_with(QMD_PENDING_MARKER_PREFIX) || !name.ends_with(QMD_PENDING_MARKER_SUFFIX) {
        return Ok(false);
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("read QMD pending marker {}", path.display()))?;
    let mut lines = content.lines();
    if lines.next() != Some(QMD_PENDING_MARKER_MAGIC) {
        anyhow::bail!(
            "managed QMD pending marker is malformed: {}",
            path.display()
        );
    }
    let timestamp = lines
        .next()
        .context("managed QMD pending marker has no timestamp")?;
    timestamp
        .parse::<u64>()
        .with_context(|| format!("parse QMD pending marker timestamp in {}", path.display()))?;
    if lines.next().is_some() {
        anyhow::bail!(
            "managed QMD pending marker has extra data: {}",
            path.display()
        );
    }
    Ok(true)
}

fn qmd_refresh_state_dir(paths: &AgentPaths) -> PathBuf {
    paths.home.join(".agent-sync").join("state")
}

fn qmd_pending_dir(paths: &AgentPaths) -> PathBuf {
    qmd_refresh_state_dir(paths).join("qmd-pending")
}

fn unix_millis(time: SystemTime) -> Result<u64> {
    time.duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .try_into()
        .context("Unix timestamp does not fit in u64")
}

fn safe_filename(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MANAGED_COMMAND: &str =
        "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/opt/agent-sync' cursor-history export";

    #[test]
    fn cursor_hook_merge_preserves_existing_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hooks.json");
        fs::write(
            &path,
            r#"{"version":1,"custom":true,"hooks":{"stop":[{"command":"existing"}],"afterFileEdit":[{"command":"format"}]}}"#,
        )
        .unwrap();

        let (content, changed) = render_cursor_hooks(&path, TEST_MANAGED_COMMAND).unwrap();
        assert!(changed);
        let value: Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(value["custom"], true);
        assert_eq!(value["hooks"]["stop"][0]["command"], "existing");
        assert_eq!(value["hooks"]["stop"][1]["command"], TEST_MANAGED_COMMAND);
        assert_eq!(
            value["hooks"]["stop"][1]["timeout"],
            CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS
        );
        assert_eq!(value["hooks"]["afterFileEdit"][0]["command"], "format");
    }

    #[test]
    fn cursor_hook_repairs_only_the_managed_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hooks.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "hooks": {
    "stop": [
      {"command": "custom cursor-history export", "timeout": 17},
      {
        "command": "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/custom' cursor-history export --custom",
        "timeout": 19
      },
      {
        "command": "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/old/agent-sync' cursor-history export",
        "timeout": 30,
        "failClosed": false
      }
    ]
  }
}"#,
        )
        .unwrap();

        let (content, changed) = render_cursor_hooks(&path, TEST_MANAGED_COMMAND).unwrap();
        assert!(changed);
        let value: Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(
            value["hooks"]["stop"][0],
            json!({"command": "custom cursor-history export", "timeout": 17})
        );
        assert_eq!(
            value["hooks"]["stop"][1],
            json!({
                "command": "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/custom' cursor-history export --custom",
                "timeout": 19
            })
        );
        assert_eq!(value["hooks"]["stop"][2]["command"], TEST_MANAGED_COMMAND);
        assert_eq!(
            value["hooks"]["stop"][2]["timeout"],
            CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS
        );
        assert_eq!(value["hooks"]["stop"][2]["failClosed"], false);
    }

    #[test]
    fn cursor_hook_refuses_an_unsupported_schema_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hooks.json");
        fs::write(
            &path,
            format!(
                "{{\"version\":null,\"hooks\":{{\"stop\":[{{\"command\":{},\"timeout\":{CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS}}}]}}}}",
                serde_json::to_string(TEST_MANAGED_COMMAND).unwrap()
            ),
        )
        .unwrap();

        let error = render_cursor_hooks(&path, TEST_MANAGED_COMMAND).unwrap_err();

        assert!(error.to_string().contains("numeric value 1"));
        assert_eq!(
            serde_json::from_str::<Value>(&fs::read_to_string(path).unwrap()).unwrap()["version"],
            Value::Null
        );
    }

    #[test]
    fn cursor_transcript_exports_to_searchable_markdown() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = paths
            .cursor_home
            .join("projects/example/agent-transcripts/session/session.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            concat!(
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
                "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\"},{\"type\":\"text\",\"text\":\"hi\"}]}}\n"
            ),
        )
        .unwrap();
        let output_dir = temp.path().join("sessions");
        let hook = json!({
            "conversation_id": "session",
            "model": "cursor-model",
            "workspace_roots": ["/example"],
            "transcript_path": transcript,
        });

        let output = export_cursor_history(&paths, &hook, Some(output_dir), false)
            .unwrap()
            .unwrap();
        let markdown = fs::read_to_string(output).unwrap();
        assert!(markdown.contains("source: cursor"));
        assert!(markdown.contains("## User\n\nhello"));
        assert!(markdown.contains("## Cursor\n\nhi"));
        assert!(!markdown.contains("tool_use"));
    }

    #[test]
    fn cursor_subagent_transcripts_without_ids_do_not_overwrite_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let output_dir = temp.path().join("sessions");
        let transcripts = [
            paths
                .cursor_home
                .join("projects/example/chat-one/subagents/worker.jsonl"),
            paths
                .cursor_home
                .join("projects/example/chat-two/subagents/worker.jsonl"),
        ];
        for transcript in &transcripts {
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(
                transcript,
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
            )
            .unwrap();
        }

        let first = export_cursor_history(
            &paths,
            &json!({"transcript_path": transcripts[0]}),
            Some(output_dir.clone()),
            false,
        )
        .unwrap()
        .unwrap();
        let second = export_cursor_history(
            &paths,
            &json!({"transcript_path": transcripts[1]}),
            Some(output_dir),
            false,
        )
        .unwrap()
        .unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
        assert_ne!(
            first.file_name().unwrap().to_string_lossy(),
            "cursor-subagents.md"
        );
    }

    #[test]
    fn cursor_history_sweep_backfills_main_and_subagent_transcripts() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let main = paths
            .cursor_home
            .join("projects/example/agent-transcripts/chat/chat.jsonl");
        let subagent = paths
            .cursor_home
            .join("projects/example/agent-transcripts/chat/subagents/worker.jsonl");
        for transcript in [&main, &subagent] {
            fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            fs::write(
                transcript,
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
            )
            .unwrap();
        }
        export_cursor_history(
            &paths,
            &json!({
                "conversation_id": "chat",
                "model": "cursor-model",
                "workspace_roots": ["/example"],
                "transcript_path": main,
            }),
            None,
            false,
        )
        .unwrap();

        let checked = sweep_cursor_history(&paths, true).unwrap();

        assert_eq!(checked, 2);
        let sessions = temp.path().join("Documents/Obsidian/sessions");
        assert!(sessions.join("cursor-chat.md").exists());
        assert!(sessions.join("cursor-chat-subagent-worker.md").exists());
        let main_export = fs::read_to_string(sessions.join("cursor-chat.md")).unwrap();
        assert!(main_export.contains("model: \"cursor-model\""));
        assert!(main_export.contains("workspace_roots: [\"/example\"]"));
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn cursor_history_refreshes_qmd_for_each_completed_chat() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);
        let hook = test_hook(&transcript);

        export_cursor_history(&paths, &hook, None, true).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "update\nembed\n");

        export_cursor_history(&paths, &hook, None, true).unwrap();
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "update\nembed\nupdate\nembed\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cursor_history_propagates_qmd_command_failures() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, Some("embed"));

        let error = export_cursor_history(&paths, &test_hook(&transcript), None, true).unwrap_err();

        assert!(error.to_string().contains("qmd embed failed"));
        assert_eq!(fs::read_to_string(log).unwrap(), "update\nembed\n");
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn cursor_history_retries_qmd_after_a_failed_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        let hook = test_hook(&transcript);
        install_test_qmd(&paths, &log, Some("embed"));

        export_cursor_history(&paths, &hook, None, true).unwrap_err();
        install_test_qmd(&paths, &log, None);
        export_cursor_history(&paths, &hook, None, true).unwrap();

        assert_eq!(
            fs::read_to_string(log).unwrap(),
            "update\nembed\nupdate\nembed\n"
        );
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 0);
    }

    #[test]
    fn cursor_history_coverage_detects_missing_and_stale_exports() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);

        let missing = cursor_history_coverage(&paths).unwrap();
        assert_eq!(missing.transcripts, 1);
        assert_eq!(missing.expected_exports.len(), 1);
        assert_eq!(missing.missing, missing.expected_exports);
        assert!(missing.stale.is_empty());

        export_cursor_history(&paths, &test_hook(&transcript), None, false).unwrap();
        assert!(cursor_history_coverage(&paths).unwrap().is_complete());

        let mut transcript_file = OpenOptions::new().append(true).open(&transcript).unwrap();
        writeln!(
            transcript_file,
            "{}",
            json!({"role":"user","message":{"content":[{"type":"text","text":"new turn"}]}})
        )
        .unwrap();
        transcript_file.sync_all().unwrap();

        let stale = cursor_history_coverage(&paths).unwrap();
        assert!(stale.missing.is_empty());
        assert_eq!(stale.stale, stale.expected_exports);
    }

    #[test]
    fn cursor_transcript_schema_drift_is_unhealthy_instead_of_silent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        fs::write(&transcript, "{\"newCursorSchema\":true}\n").unwrap();

        let coverage = cursor_history_coverage(&paths).unwrap();
        assert_eq!(coverage.transcripts, 1);
        assert_eq!(coverage.unreadable.len(), 1);
        assert!(!coverage.is_complete());

        let error =
            export_cursor_history(&paths, &test_hook(&transcript), None, false).unwrap_err();
        assert!(error
            .to_string()
            .contains("Cursor may have changed its transcript format"));
    }

    #[test]
    fn partial_cursor_transcript_schema_drift_is_unhealthy() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let mut transcript_file = OpenOptions::new().append(true).open(&transcript).unwrap();
        writeln!(
            transcript_file,
            "{}",
            json!({"role":"user","message":{"content":{"newCursorSchema":true}}})
        )
        .unwrap();
        transcript_file.sync_all().unwrap();

        let coverage = cursor_history_coverage(&paths).unwrap();
        assert_eq!(coverage.transcripts, 1);
        assert_eq!(coverage.unreadable.len(), 1);
        assert!(!coverage.is_complete());

        let error =
            export_cursor_history(&paths, &test_hook(&transcript), None, false).unwrap_err();
        assert!(error
            .to_string()
            .contains("Cursor may have changed its transcript format"));
    }

    #[test]
    fn cursor_history_refuses_to_replace_an_unowned_export() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let output = paths
            .home
            .join("Documents/Obsidian/sessions/cursor-session.md");
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::write(&output, "personal note\n").unwrap();

        let error =
            export_cursor_history(&paths, &test_hook(&transcript), None, false).unwrap_err();

        assert!(error.to_string().contains("target-owned file"));
        assert_eq!(fs::read_to_string(output).unwrap(), "personal note\n");
    }

    #[test]
    fn cursor_history_rejects_a_mismatched_hook_conversation_id() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let hook = json!({
            "conversation_id": "another-chat",
            "transcript_path": transcript,
        });

        let error = export_cursor_history(&paths, &hook, None, false).unwrap_err();

        assert!(error.to_string().contains("does not match transcript id"));
        assert!(!paths
            .home
            .join("Documents/Obsidian/sessions/cursor-another-chat.md")
            .exists());
    }

    #[test]
    fn qmd_patterns_must_cover_cursor_exports() {
        for pattern in ["*.md", "cursor-*.md", "**/*.md", "**/cursor-*.md"] {
            assert!(qmd_pattern_covers_cursor_exports(pattern), "{pattern}");
        }
        for pattern in ["codex-*.md", "notes/*.md", "cursor-?.md"] {
            assert!(!qmd_pattern_covers_cursor_exports(pattern), "{pattern}");
        }
    }

    #[test]
    fn qmd_status_omits_pending_row_when_nothing_needs_embedding() {
        assert_eq!(
            parse_qmd_pending_embeddings("Documents\n  Vectors:  747465 embedded\n").unwrap(),
            0
        );
        assert_eq!(
            parse_qmd_pending_embeddings(
                "Documents\n  Vectors:  747000 embedded\n  Pending:  7 need embedding\n"
            )
            .unwrap(),
            7
        );
        assert!(parse_qmd_pending_embeddings("QMD Status\n").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn qmd_health_requires_included_cursor_exports() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let log = temp.path().join("qmd.log");

        install_test_qmd_with_config(&paths, &log, None, "codex-*.md", "yes (default)", true);
        let error = qmd_health(&paths).unwrap_err();
        assert!(error.to_string().contains("does not cover cursor-*.md"));

        install_test_qmd_with_config(&paths, &log, None, "**/*.md", "no", true);
        let error = qmd_health(&paths).unwrap_err();
        assert!(error.to_string().contains("excluded from global search"));

        install_test_qmd(&paths, &log, None);
        let health = qmd_health(&paths).unwrap();
        assert!(health.sessions_included);
        assert_eq!(health.sessions_pattern, "**/*.md");
        assert_eq!(health.pending_embeddings, 0);
    }

    #[cfg(unix)]
    #[test]
    fn hook_refresh_leaves_marker_when_refresh_is_active() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);
        let refresh_lock = acquire_qmd_refresh_lock(&paths).unwrap().unwrap();

        let output = export_cursor_history(&paths, &test_hook(&transcript), None, true)
            .unwrap()
            .unwrap();

        assert!(output.exists());
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 1);
        assert!(!log.exists());
        drop(refresh_lock);
    }

    #[test]
    fn qmd_pending_cleanup_preserves_new_and_unrelated_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let pending = qmd_pending_dir(&paths);
        fs::create_dir_all(&pending).unwrap();
        let unrelated = pending.join("personal-note.txt");
        fs::write(&unrelated, "keep me\n").unwrap();
        write_qmd_pending_marker(&paths, "same-chat").unwrap();
        let refresh_started = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        write_qmd_pending_marker(&paths, "same-chat").unwrap();

        assert_eq!(qmd_pending_exports(&paths).unwrap(), 2);
        clear_qmd_pending_markers(&paths, refresh_started).unwrap();

        assert_eq!(qmd_pending_exports(&paths).unwrap(), 1);
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep me\n");
    }

    #[cfg(unix)]
    #[test]
    fn qmd_pending_symlink_is_refused_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let victim = temp.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        let unrelated = victim.join("personal-note.txt");
        fs::write(&unrelated, "keep me\n").unwrap();
        fs::create_dir_all(qmd_refresh_state_dir(&paths)).unwrap();
        symlink(&victim, qmd_pending_dir(&paths)).unwrap();

        let error = clear_qmd_pending_markers(&paths, SystemTime::now()).unwrap_err();

        assert!(error.to_string().contains("symlinked QMD pending"));
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep me\n");
    }

    #[cfg(unix)]
    #[test]
    fn forced_refresh_waits_then_verifies_every_export() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);
        export_cursor_history(&paths, &test_hook(&transcript), None, false).unwrap();
        write_qmd_pending_marker(&paths, "session").unwrap();
        let refresh_lock = acquire_qmd_refresh_lock(&paths).unwrap().unwrap();
        let refresh_paths = paths.clone();

        let refresh = std::thread::spawn(move || refresh_qmd_index(&refresh_paths, true));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!refresh.is_finished());
        drop(refresh_lock);

        assert!(refresh.join().unwrap().unwrap());
        assert_eq!(fs::read_to_string(log).unwrap(), "update\nembed\n");
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 0);
        assert!(qmd_refresh_last_success(&paths).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_keeps_marker_when_export_is_not_retrievable() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd_with_config(&paths, &log, None, "**/*.md", "yes (default)", false);
        export_cursor_history(&paths, &test_hook(&transcript), None, false).unwrap();
        write_qmd_pending_marker(&paths, "session").unwrap();

        let error = refresh_qmd_index(&paths, true).unwrap_err();

        assert!(error.to_string().contains("not retrievable from QMD"));
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 1);
        assert!(qmd_refresh_last_success(&paths).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cursor_history_hook_refuses_a_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        fs::create_dir_all(&paths.cursor_home).unwrap();
        let target = temp.path().join("real-hooks.json");
        fs::write(&target, "{\"version\":1}\n").unwrap();
        symlink(&target, paths.cursor_home.join("hooks.json")).unwrap();

        let error =
            install_cursor_history_hook(&paths, Path::new("/opt/agent-sync"), false).unwrap_err();
        assert!(error.to_string().contains("symlinked Cursor hooks"));
        assert_eq!(fs::read_to_string(target).unwrap(), "{\"version\":1}\n");
    }

    #[test]
    fn qmd_refresh_lock_and_state_are_shared() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());

        let lock = acquire_qmd_refresh_lock(&paths).unwrap().unwrap();
        assert!(acquire_qmd_refresh_lock(&paths).unwrap().is_none());
        drop(lock);
        assert!(acquire_qmd_refresh_lock(&paths).unwrap().is_some());

        write_qmd_refresh_state(&paths, SystemTime::now()).unwrap();
        assert!(qmd_refresh_last_success(&paths).unwrap().is_some());
    }

    #[test]
    fn stale_lock_cleanup_does_not_remove_a_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("refresh.lock");
        fs::write(&path, "4294967295:0\n").unwrap();
        let stale = read_lock_snapshot(&path).unwrap().unwrap();
        write_atomic(&path, b"replacement:1\n").unwrap();

        assert!(!remove_lock_if_snapshot_matches(&path, &stale).unwrap());
        assert_eq!(fs::read_to_string(path).unwrap(), "replacement:1\n");
    }

    #[test]
    fn stale_lock_cleanup_removes_the_same_dead_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("refresh.lock");
        fs::write(&path, "4294967295:0\n").unwrap();

        assert!(remove_stale_lock_if_unchanged(&path).unwrap());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_history_lock_symlinks_are_refused() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let state = qmd_refresh_state_dir(&paths);
        fs::create_dir_all(&state).unwrap();
        let missing = temp.path().join("missing-lock-target");
        symlink(&missing, state.join("cursor-history-export.lock")).unwrap();

        let export_error = acquire_cursor_history_export_lock(&paths)
            .err()
            .expect("dangling export lock symlink must fail");
        assert!(export_error.to_string().contains("lock path is a symlink"));

        fs::remove_file(state.join("cursor-history-export.lock")).unwrap();
        symlink(&missing, state.join("qmd-refresh.lock")).unwrap();
        let qmd_error = acquire_qmd_refresh_lock(&paths)
            .err()
            .expect("dangling QMD lock symlink must fail");
        assert!(qmd_error.to_string().contains("lock path is a symlink"));
    }

    fn write_test_transcript(paths: &AgentPaths) -> PathBuf {
        let transcript = paths
            .cursor_home
            .join("projects/example/agent-transcripts/session/session.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            concat!(
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
                "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n"
            ),
        )
        .unwrap();
        transcript
    }

    fn test_hook(transcript: &Path) -> Value {
        json!({
            "conversation_id": "session",
            "model": "cursor-model",
            "workspace_roots": ["/example"],
            "transcript_path": transcript,
        })
    }

    #[cfg(unix)]
    fn install_test_qmd(paths: &AgentPaths, log: &Path, fail_on: Option<&str>) {
        install_test_qmd_with_config(paths, log, fail_on, "**/*.md", "yes (default)", true);
    }

    #[cfg(unix)]
    fn install_test_qmd_with_config(
        paths: &AgentPaths,
        log: &Path,
        fail_on: Option<&str>,
        pattern: &str,
        include: &str,
        retrieve_exports: bool,
    ) {
        use std::os::unix::fs::PermissionsExt;

        let qmd = paths.home.join(".local/bin/qmd");
        let sessions = paths.home.join("Documents/Obsidian/sessions");
        let export_lock = qmd_refresh_state_dir(paths).join("cursor-history-export.lock");
        fs::create_dir_all(qmd.parent().unwrap()).unwrap();
        let failure = fail_on
            .map(|subcommand| format!("[ \"$1\" = \"{subcommand}\" ] && exit 9\n"))
            .unwrap_or_default();
        let multi_get_result = if retrieve_exports {
            format!(
                concat!(
                    "name=${{2##*/}}\n",
                    "name=${{name%,}}\n",
                    "target={}/\"$name\"\n",
                    "[ -f \"$target\" ] || {{ printf '[]\\n'; exit 0; }}\n",
                    "printf '[{{\"file\":\"qmd://sessions/%s\",\"title\":\"test\",\"body\":\"' \"$name\"\n",
                    "sed -e 's/\\\\/\\\\\\\\/g' -e 's/\"/\\\\\"/g' -e 's/$/\\\\n/' \"$target\" | tr -d '\\n'\n",
                    "printf '\"}}]\\n'\n"
                ),
                shell_quote(&sessions.to_string_lossy())
            )
        } else {
            "printf '[]\\n'\n".to_string()
        };
        fs::write(
            &qmd,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "case \"$1\" in\n",
                    "update|embed)\n",
                    "  printf '%s\\n' \"$1\" >> {}\n",
                    "  if [ -e {} ]; then echo 'export lock held during QMD refresh' >&2; exit 8; fi\n",
                    "  {failure}",
                    "  exit 0\n",
                    "  ;;\n",
                    "collection)\n",
                    "  printf '%s\\n' 'Collection: sessions' '  Path:     {}' '  Pattern:  {}' '  Include:  {}'\n",
                    "  exit 0\n",
                    "  ;;\n",
                    "status)\n",
                    "  printf '%s\\n' '  Pending:  0 need embedding'\n",
                    "  exit 0\n",
                    "  ;;\n",
                    "multi-get)\n",
                    "  {multi_get_result}",
                    "  ;;\n",
                    "*) exit 9 ;;\n",
                    "esac\n"
                ),
                shell_quote(&log.to_string_lossy()),
                shell_quote(&export_lock.to_string_lossy()),
                sessions.display(),
                pattern,
                include,
                failure = failure,
                multi_get_result = multi_get_result,
            ),
        )
        .unwrap();
        fs::set_permissions(&qmd, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

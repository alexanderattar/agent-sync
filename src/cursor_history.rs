use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error as StdError,
    fmt,
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::{
    adapters::AgentPaths,
    fsx::{
        ensure_dir, hash_bytes, read_to_string_if_exists, redact_known_secrets,
        replace_file_with_backup_if_unchanged, write_atomic,
    },
};

const CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS: u64 = 30;
const MANAGED_HOOK_COMMAND_PREFIX: &str = "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 ";
const MANAGED_HOOK_COMMAND_SUFFIX: &str = " cursor-history export";
const MANAGED_HOOK_SKIP_QMD_COMMAND_SUFFIX: &str = " cursor-history export --skip-qmd";
const MANAGED_HOOK_V1_SUFFIX: &str = " # agent-sync-managed-hook-v1";
const QMD_REFRESH_LOCK_STALE_AFTER: Duration = Duration::from_secs(10 * 60);
const PERSISTENT_LOCK_HARD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const QMD_REFRESH_LOCK_WAIT: Duration = Duration::from_millis(500);
const QMD_DEFERRED_REFRESH_DELAY: Duration = Duration::from_secs(2);
const CURSOR_EXPORT_LOCK_MAX_WAITS: usize = 240;
const QMD_FORCE_LOCK_MAX_WAITS: usize = 1_200;
const QMD_PENDING_MARKER_PREFIX: &str = "agent-sync-qmd-";
const QMD_PENDING_MARKER_SUFFIX: &str = ".pending";
const QMD_PENDING_MARKER_MAGIC_V1: &str = "agent-sync-qmd-pending-v1";
const QMD_PENDING_MARKER_MAGIC_V2: &str = "agent-sync-qmd-pending-v2";
const QMD_EXACT_LOOKUP_MAX_EXPORTS: usize = 64;
const QMD_EXACT_LOOKUP_MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
pub const QMD_CURSOR_COLLECTION: &str = "agent-sync-cursor";

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CursorHistorySweepReport {
    pub exported: usize,
    pub unreadable: Vec<PathBuf>,
}

#[derive(Debug)]
struct CursorTranscriptFormatError {
    message: String,
}

impl fmt::Display for CursorTranscriptFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for CursorTranscriptFormatError {}

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
    install_cursor_history_hook_with_refresh(
        paths,
        executable,
        &default_cursor_history_output_dir(paths),
        dry_run,
        true,
    )
}

pub fn install_cursor_history_hook_with_refresh(
    paths: &AgentPaths,
    executable: &Path,
    output_dir: &Path,
    dry_run: bool,
    refresh_qmd: bool,
) -> Result<CursorHistoryInstallReport> {
    let hooks_path = paths.cursor_home.join("hooks.json");
    ensure_cursor_hooks_write_safe(&hooks_path)?;
    let command = managed_hook_command(paths, executable, output_dir, refresh_qmd)?;
    let (_, changed) = render_cursor_hooks(&hooks_path, &command)?;
    if dry_run || !changed {
        return Ok(CursorHistoryInstallReport {
            changed,
            dry_run,
            hooks_path,
            backup: None,
        });
    }

    let backup_root = paths
        .home
        .join(".agent-sync")
        .join("backups")
        .join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    let (changed, backup) = apply_cursor_hook_edit(paths, &hooks_path, &backup_root, || {
        render_cursor_hooks(&hooks_path, &command)
    })?;
    Ok(CursorHistoryInstallReport {
        changed,
        dry_run,
        hooks_path,
        backup,
    })
}

fn managed_hook_command(
    paths: &AgentPaths,
    executable: &Path,
    output_dir: &Path,
    refresh_qmd: bool,
) -> Result<String> {
    let path_arguments = [
        ("--home", paths.home.as_path()),
        ("--codex-home", paths.codex_home.as_path()),
        ("--claude-home", paths.claude_home.as_path()),
        ("--claude-config", paths.claude_config.as_path()),
        ("--cursor-home", paths.cursor_home.as_path()),
        ("--cursor-config", paths.cursor_config.as_path()),
        ("--agents-home", paths.agents_home.as_path()),
    ];
    let executable = executable.to_str().with_context(|| {
        format!(
            "agent-sync executable is not valid UTF-8: {}",
            executable.display()
        )
    })?;
    let mut command = format!("{MANAGED_HOOK_COMMAND_PREFIX}{}", shell_quote(executable));
    for (flag, path) in path_arguments {
        let value = path
            .to_str()
            .with_context(|| format!("{flag} path is not valid UTF-8: {}", path.display()))?;
        command.push(' ');
        command.push_str(flag);
        command.push(' ');
        command.push_str(&shell_quote(value));
    }
    command.push_str(MANAGED_HOOK_COMMAND_SUFFIX);
    command.push_str(" --output-dir ");
    command.push_str(&shell_quote(output_dir.to_str().with_context(|| {
        format!(
            "Cursor history output path is not valid UTF-8: {}",
            output_dir.display()
        )
    })?));
    if !refresh_qmd {
        command.push_str(" --skip-qmd");
    }
    command.push_str(MANAGED_HOOK_V1_SUFFIX);
    Ok(command)
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

    let backup_root = paths
        .home
        .join(".agent-sync")
        .join("backups")
        .join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    let (changed, backup) = apply_cursor_hook_edit(paths, &hooks_path, &backup_root, || {
        render_cursor_hooks_without_managed(&hooks_path)
    })?;
    Ok(CursorHistoryRemoveReport {
        changed,
        dry_run: false,
        hooks_path,
        backup,
    })
}

fn apply_cursor_hook_edit<F>(
    paths: &AgentPaths,
    hooks_path: &Path,
    backup_root: &Path,
    mut render: F,
) -> Result<(bool, Option<PathBuf>)>
where
    F: FnMut() -> Result<(Vec<u8>, bool)>,
{
    for _ in 0..3 {
        let expected = read_bytes_if_exists(hooks_path)?;
        let (content, changed) = render()?;
        if !changed {
            return Ok((false, None));
        }
        if read_bytes_if_exists(hooks_path)? != expected {
            continue;
        }
        let backup = replace_file_with_backup_if_unchanged(
            backup_root,
            &paths.cursor_home,
            hooks_path,
            expected.as_deref(),
            &content,
        )?;
        return Ok((true, backup));
    }
    anyhow::bail!("Cursor hooks changed repeatedly while applying; retry the sync")
}

fn read_bytes_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
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
    let lock_mode = if running_from_cursor_hook() {
        CursorHistoryLockMode::SkipIfBusy
    } else {
        CursorHistoryLockMode::Wait
    };
    export_cursor_history_with_lock_mode(paths, hook, output_dir, refresh_qmd, lock_mode)
}

#[derive(Clone, Copy)]
enum CursorHistoryLockMode {
    Wait,
    SkipIfBusy,
}

fn export_cursor_history_with_lock_mode(
    paths: &AgentPaths,
    hook: &Value,
    output_dir: Option<PathBuf>,
    refresh_qmd: bool,
    lock_mode: CursorHistoryLockMode,
) -> Result<Option<PathBuf>> {
    let Some(transcript_path) = hook.get("transcript_path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let export_lock = acquire_cursor_history_export_lock_for_mode(paths, lock_mode)?;
    let Some(export_lock) = export_lock else {
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
    let Some(turns) = supported_cursor_turns(&transcript_path, &raw)? else {
        return Ok(None);
    };

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
    let output_dir = output_dir.unwrap_or_else(|| default_cursor_history_output_dir(paths));
    ensure_private_history_dir(&output_dir)?;
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
    let (safe_turns, redaction_count) = redact_turns(turns);
    let content = render_markdown(
        &conversation_id,
        timestamp,
        &model,
        &workspace_roots,
        redaction_count,
        &safe_turns,
    )?;

    let changed = existing.as_deref() != Some(content.as_str());
    if changed {
        write_atomic(&output, content.as_bytes())?;
    }
    set_private_file_mode(&output)?;
    drop(export_lock);
    refresh_qmd_after_cursor_export(paths, &output_dir, &output, refresh_qmd, changed)?;
    Ok(Some(output))
}

fn refresh_qmd_after_cursor_export(
    paths: &AgentPaths,
    output_dir: &Path,
    output: &Path,
    refresh_qmd: bool,
    changed: bool,
) -> Result<()> {
    if !refresh_qmd {
        return Ok(());
    }
    let refresh_needed = if changed {
        write_qmd_pending_marker(paths, output)?;
        true
    } else {
        has_pending_qmd_work(paths, output_dir)?
    };
    if !refresh_needed {
        return Ok(());
    }
    if running_from_cursor_hook() {
        enqueue_deferred_qmd_refresh(paths, output_dir)?;
    } else {
        refresh_pending_qmd_index_for_output(paths, output_dir, false)?;
    }
    Ok(())
}

/// Exports every readable Cursor agent transcript so a scheduled sync can
/// recover sessions missed by the stop hook.
pub fn sweep_cursor_history(paths: &AgentPaths, mark_qmd_pending: bool) -> Result<usize> {
    sweep_cursor_history_to(
        paths,
        &default_cursor_history_output_dir(paths),
        mark_qmd_pending,
    )
}

pub fn sweep_cursor_history_to(
    paths: &AgentPaths,
    output_dir: &Path,
    mark_qmd_pending: bool,
) -> Result<usize> {
    Ok(sweep_cursor_history_report_to(paths, output_dir, mark_qmd_pending)?.exported)
}

pub(crate) fn sweep_cursor_history_report_to(
    paths: &AgentPaths,
    output_dir: &Path,
    mark_qmd_pending: bool,
) -> Result<CursorHistorySweepReport> {
    let mut exported = 0;
    let mut unreadable = Vec::new();
    for transcript in cursor_transcript_paths(paths)? {
        let hook = json!({"transcript_path": &transcript});
        match export_cursor_history(paths, &hook, Some(output_dir.to_path_buf()), false) {
            Ok(Some(output)) => {
                exported += 1;
                if mark_qmd_pending {
                    write_qmd_pending_marker(paths, &output)?;
                }
            }
            Ok(None) => {}
            Err(error) if is_cursor_transcript_format_error(&error) => unreadable.push(transcript),
            Err(error) => return Err(error),
        }
    }
    Ok(CursorHistorySweepReport {
        exported,
        unreadable,
    })
}

pub(crate) fn cursor_history_unreadable_count(paths: &AgentPaths) -> Result<usize> {
    Ok(cursor_transcript_paths(paths)?
        .into_iter()
        .filter(|transcript| cursor_transcript_is_unreadable(transcript))
        .count())
}

fn cursor_transcript_is_unreadable(transcript: &Path) -> bool {
    match fs::read_to_string(transcript) {
        Ok(raw) => supported_cursor_turns(transcript, &raw).is_err(),
        Err(_) => true,
    }
}

pub fn cursor_history_coverage(paths: &AgentPaths) -> Result<CursorHistoryCoverage> {
    cursor_history_coverage_at(paths, &default_cursor_history_output_dir(paths))
}

pub fn cursor_history_coverage_at(
    paths: &AgentPaths,
    output_dir: &Path,
) -> Result<CursorHistoryCoverage> {
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
        let (turns, redaction_count) = redact_turns(turns);
        let expected = render_markdown(
            &conversation_id,
            timestamp,
            &model,
            &workspace_roots,
            redaction_count,
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

fn supported_cursor_turns(
    transcript_path: &Path,
    raw: &str,
) -> Result<Option<Vec<(String, String)>>> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let turns = cursor_turns(raw).map_err(|error| CursorTranscriptFormatError {
        message: format!(
            "Cursor transcript {} has an unsupported entry; Cursor may have changed its transcript format: {error:#}",
            transcript_path.display()
        ),
    })?;
    if turns.is_empty() {
        return Err(CursorTranscriptFormatError {
            message: format!(
                "Cursor transcript {} contains no supported user or assistant turns; Cursor may have changed its transcript format",
                transcript_path.display()
            ),
        }
        .into());
    }
    Ok(Some(turns))
}

fn is_cursor_transcript_format_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<CursorTranscriptFormatError>())
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
    secret_redactions: usize,
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
    out.push_str(&format!("secret_redactions: {secret_redactions}\n"));
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

fn redact_turns(turns: Vec<(String, String)>) -> (Vec<(String, String)>, usize) {
    let mut redaction_count = 0;
    let turns = turns
        .into_iter()
        .map(|(role, text)| {
            let (text, count) = redact_known_secrets(&text);
            redaction_count += count;
            (role, text)
        })
        .collect();
    (turns, redaction_count)
}

fn is_managed_hook(entry: &Value) -> bool {
    let Some(command) = entry
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| command.strip_prefix(MANAGED_HOOK_COMMAND_PREFIX))
    else {
        return false;
    };
    let legacy = [
        MANAGED_HOOK_COMMAND_SUFFIX,
        MANAGED_HOOK_SKIP_QMD_COMMAND_SUFFIX,
    ]
    .iter()
    .any(|suffix| {
        command
            .strip_suffix(suffix)
            .is_some_and(|executable| !executable.is_empty())
    });
    let current = command.ends_with(MANAGED_HOOK_V1_SUFFIX)
        && command.contains(" cursor-history export --output-dir ");
    legacy || current
}

pub fn refresh_qmd_index(paths: &AgentPaths, force: bool) -> Result<bool> {
    refresh_qmd_index_for_output(paths, &default_cursor_history_output_dir(paths), force)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QmdVerificationScope {
    Full,
    Pending,
}

pub fn refresh_qmd_index_for_output(
    paths: &AgentPaths,
    output_dir: &Path,
    force: bool,
) -> Result<bool> {
    refresh_qmd_index_for_output_with_scope(paths, output_dir, force, QmdVerificationScope::Full)
}

pub(crate) fn refresh_pending_qmd_index_for_output(
    paths: &AgentPaths,
    output_dir: &Path,
    force: bool,
) -> Result<bool> {
    refresh_qmd_index_for_output_with_scope(paths, output_dir, force, QmdVerificationScope::Pending)
}

fn refresh_qmd_index_for_output_with_scope(
    paths: &AgentPaths,
    output_dir: &Path,
    force: bool,
    scope: QmdVerificationScope,
) -> Result<bool> {
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
    let pending_work = pending_qmd_work(paths, output_dir)?;
    if scope == QmdVerificationScope::Pending && pending_work.markers.is_empty() {
        return Ok(false);
    }

    let qmd = qmd_executable(paths).context("QMD executable was not found in a standard path")?;
    run_qmd_command(&qmd, "update")?;
    run_qmd_command(&qmd, "embed")?;

    let health = qmd_health(paths)?;
    let expected_sessions = output_dir.to_path_buf();
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
    let exports = if scope == QmdVerificationScope::Full || pending_work.has_legacy_marker {
        let coverage = cursor_history_coverage_at(paths, output_dir)?;
        if !coverage.is_complete() {
            anyhow::bail!(
                "Cursor history coverage is incomplete after refresh: {} missing, {} stale, {} unreadable",
                coverage.missing.len(),
                coverage.stale.len(),
                coverage.unreadable.len()
            );
        }
        coverage.expected_exports
    } else {
        pending_work.exports.clone()
    };
    let missing = qmd_missing_exports(paths, &exports)?;
    if !missing.is_empty() {
        anyhow::bail!(
            "{} Cursor history export(s) are not retrievable from QMD: {}",
            missing.len(),
            missing
                .iter()
                .map(|export| export.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    write_qmd_refresh_state(paths, SystemTime::now())?;
    clear_qmd_pending_markers(&pending_work.markers)?;
    Ok(true)
}

pub fn qmd_executable(paths: &AgentPaths) -> Option<PathBuf> {
    let fixed = [
        paths.home.join(".local/bin/qmd"),
        PathBuf::from("/usr/local/bin/qmd"),
        PathBuf::from("/opt/homebrew/bin/qmd"),
    ];
    fixed.into_iter().find(|path| path.is_file()).or_else(|| {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .map(|directory| directory.join("qmd"))
            .find(|path| path.is_file())
    })
}

pub fn default_cursor_history_output_dir(paths: &AgentPaths) -> PathBuf {
    paths.home.join(".agent-sync/history/cursor")
}

fn ensure_private_history_dir(path: &Path) -> Result<()> {
    ensure_dir(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect Cursor history directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "Cursor history path is not a private regular directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect Cursor history directory {}", path.display()))?;
    }
    Ok(())
}

fn set_private_file_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("protect Cursor history export {}", path.display()))?;
    }
    Ok(())
}

fn running_from_cursor_hook() -> bool {
    std::env::var_os("AGENT_SYNC_CURSOR_HISTORY_HOOK").as_deref() == Some(std::ffi::OsStr::new("1"))
}

pub fn enqueue_deferred_qmd_refresh(paths: &AgentPaths, output_dir: &Path) -> Result<bool> {
    let state_dir = qmd_refresh_state_dir(paths);
    ensure_dir(&state_dir)?;
    let reservation_path = state_dir.join("qmd-refresh-deferred.lock");
    for _ in 0..2 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&reservation_path)
        {
            Ok(mut file) => {
                writeln!(file, "deferred:{}", unix_millis(SystemTime::now())?)
                    .with_context(|| format!("write {}", reservation_path.display()))?;
                file.sync_all()?;
                let snapshot = read_lock_snapshot(&reservation_path)?.with_context(|| {
                    format!(
                        "deferred QMD reservation changed at {}",
                        reservation_path.display()
                    )
                })?;
                if let Err(error) = spawn_qmd_refresh_process(paths, output_dir) {
                    let cleanup = remove_lock_if_snapshot_matches(&reservation_path, &snapshot);
                    if let Err(cleanup_error) = cleanup {
                        anyhow::bail!(
                            "start deferred QMD refresh failed: {error:#}; reservation cleanup also failed: {cleanup_error:#}"
                        );
                    }
                    return Err(error);
                }
                return Ok(true);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let Some(snapshot) = read_lock_snapshot(&reservation_path)? else {
                    continue;
                };
                let age = SystemTime::now()
                    .duration_since(snapshot.identity.modified)
                    .unwrap_or_default();
                if age > QMD_REFRESH_LOCK_STALE_AFTER {
                    remove_lock_if_snapshot_matches(&reservation_path, &snapshot)?;
                    continue;
                }
                return Ok(false);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create {}", reservation_path.display()));
            }
        }
    }
    Ok(false)
}

fn spawn_qmd_refresh_process(paths: &AgentPaths, output_dir: &Path) -> Result<()> {
    let executable = std::env::current_exe().context("resolve agent-sync executable")?;
    let mut command = Command::new(executable);
    for (flag, path) in [
        ("--home", paths.home.as_path()),
        ("--codex-home", paths.codex_home.as_path()),
        ("--claude-home", paths.claude_home.as_path()),
        ("--claude-config", paths.claude_config.as_path()),
        ("--cursor-home", paths.cursor_home.as_path()),
        ("--cursor-config", paths.cursor_config.as_path()),
        ("--agents-home", paths.agents_home.as_path()),
    ] {
        command.arg(flag).arg(path);
    }
    command
        .args(["cursor-history", "refresh-qmd", "--output-dir"])
        .arg(output_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("AGENT_SYNC_CURSOR_HISTORY_HOOK")
        .spawn()
        .context("start deferred QMD refresh")?;
    Ok(())
}

pub fn run_deferred_qmd_refresh(paths: &AgentPaths, output_dir: &Path) -> Result<bool> {
    let reservation_path = qmd_refresh_state_dir(paths).join("qmd-refresh-deferred.lock");
    let Some(snapshot) = read_lock_snapshot(&reservation_path)? else {
        return Ok(false);
    };
    let reservation = DeferredRefreshReservation {
        path: reservation_path,
        snapshot,
    };
    std::thread::sleep(QMD_DEFERRED_REFRESH_DELAY);
    let refreshed = refresh_pending_qmd_index_for_output(paths, output_dir, false)?;
    let still_pending = has_pending_qmd_work(paths, output_dir)?;
    drop(reservation);
    if still_pending {
        enqueue_deferred_qmd_refresh(paths, output_dir)?;
    }
    Ok(refreshed)
}

struct DeferredRefreshReservation {
    path: PathBuf,
    snapshot: LockSnapshot,
}

impl Drop for DeferredRefreshReservation {
    fn drop(&mut self) {
        let _ = remove_lock_if_snapshot_matches(&self.path, &self.snapshot);
    }
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
        .args(["collection", "show", QMD_CURSOR_COLLECTION])
        .output()
        .context("inspect the agent-sync QMD collection")?;
    if !collection.status.success() {
        anyhow::bail!(
            "QMD collection {QMD_CURSOR_COLLECTION:?} check failed with {}: {}",
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
        .context("agent-sync QMD collection output has no path")?;
    let sessions_pattern = collection_stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Pattern:"))
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(ToString::to_string)
        .context("agent-sync QMD collection output has no pattern")?;
    let sessions_included = collection_stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Include:"))
        .map(str::trim)
        .map(|include| include.to_ascii_lowercase().starts_with("yes"))
        .context("agent-sync QMD collection output has no include state")?;
    if !sessions_included {
        anyhow::bail!("agent-sync QMD collection is excluded from global search");
    }
    if !qmd_pattern_covers_cursor_exports(&sessions_pattern) {
        anyhow::bail!(
            "agent-sync QMD collection pattern {sessions_pattern:?} does not cover cursor-*.md"
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

pub fn ensure_qmd_collection(paths: &AgentPaths, dry_run: bool) -> Result<bool> {
    let qmd = qmd_executable(paths).context("QMD executable was not found in a standard path")?;
    let output_dir = default_cursor_history_output_dir(paths);
    let inspection = Command::new(&qmd)
        .args(["collection", "show", QMD_CURSOR_COLLECTION])
        .output()
        .context("inspect the agent-sync QMD collection")?;
    if inspection.status.success() {
        let health = qmd_health(paths)?;
        let actual = fs::canonicalize(&health.sessions_path).unwrap_or(health.sessions_path);
        let expected = fs::canonicalize(&output_dir).unwrap_or(output_dir);
        if actual != expected {
            anyhow::bail!(
                "QMD collection {QMD_CURSOR_COLLECTION:?} points to {}, expected {}",
                actual.display(),
                expected.display()
            );
        }
        return Ok(false);
    }
    let stderr = String::from_utf8_lossy(&inspection.stderr);
    if !stderr.contains("Collection not found") {
        anyhow::bail!(
            "inspect QMD collection {QMD_CURSOR_COLLECTION:?} failed with {}: {}",
            inspection.status,
            stderr.trim()
        );
    }
    if dry_run {
        return Ok(true);
    }

    ensure_private_history_dir(&output_dir)?;
    let created = Command::new(qmd)
        .args(["collection", "add"])
        .arg(&output_dir)
        .args(["--name", QMD_CURSOR_COLLECTION])
        .output()
        .context("create the agent-sync QMD collection")?;
    if !created.status.success() {
        anyhow::bail!(
            "create QMD collection {QMD_CURSOR_COLLECTION:?} failed with {}: {}",
            created.status,
            String::from_utf8_lossy(&created.stderr).trim()
        );
    }
    qmd_health(paths)?;
    Ok(true)
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
    GlobMatcher::new(pattern, candidate).matches_from(0, 0)
}

struct GlobMatcher<'a> {
    pattern: &'a [u8],
    candidate: &'a [u8],
    memo: Vec<Vec<Option<bool>>>,
}

impl<'a> GlobMatcher<'a> {
    fn new(pattern: &'a [u8], candidate: &'a [u8]) -> Self {
        Self {
            pattern,
            candidate,
            memo: vec![vec![None; candidate.len() + 1]; pattern.len() + 1],
        }
    }

    fn matches_from(&mut self, pattern_index: usize, candidate_index: usize) -> bool {
        if let Some(result) = self.memo[pattern_index][candidate_index] {
            return result;
        }
        let result = self.match_uncached(pattern_index, candidate_index);
        self.memo[pattern_index][candidate_index] = Some(result);
        result
    }

    fn match_uncached(&mut self, pattern_index: usize, candidate_index: usize) -> bool {
        let Some(pattern_byte) = self.pattern.get(pattern_index).copied() else {
            return candidate_index == self.candidate.len();
        };
        match pattern_byte {
            b'*' if self.pattern.get(pattern_index + 1) == Some(&b'*') => {
                self.match_double_star(pattern_index, candidate_index)
            }
            b'*' => {
                self.match_zero_or_more(pattern_index + 1, pattern_index, candidate_index, false)
            }
            _ => self.match_single_character(pattern_index, candidate_index, pattern_byte),
        }
    }

    fn match_double_star(&mut self, pattern_index: usize, candidate_index: usize) -> bool {
        let mut next = pattern_index + 2;
        while self.pattern.get(next) == Some(&b'*') {
            next += 1;
        }
        if self.pattern.get(next) == Some(&b'/') {
            next += 1;
        }
        self.match_zero_or_more(next, pattern_index, candidate_index, true)
    }

    fn match_zero_or_more(
        &mut self,
        skip_pattern_index: usize,
        repeat_pattern_index: usize,
        candidate_index: usize,
        allow_slash: bool,
    ) -> bool {
        self.matches_from(skip_pattern_index, candidate_index)
            || (self.can_consume(candidate_index, allow_slash)
                && self.matches_from(repeat_pattern_index, candidate_index + 1))
    }

    fn can_consume(&self, candidate_index: usize, allow_slash: bool) -> bool {
        self.candidate
            .get(candidate_index)
            .is_some_and(|candidate| allow_slash || *candidate != b'/')
    }

    fn match_single_character(
        &mut self,
        pattern_index: usize,
        candidate_index: usize,
        pattern_byte: u8,
    ) -> bool {
        let Some(candidate_byte) = self.candidate.get(candidate_index) else {
            return false;
        };
        if pattern_byte != b'?' && pattern_byte != *candidate_byte {
            return false;
        }
        self.matches_from(pattern_index + 1, candidate_index + 1)
    }
}

#[derive(Debug)]
struct QmdExportLookup {
    export: PathBuf,
    virtual_path: String,
    body: String,
}

pub fn qmd_missing_exports(paths: &AgentPaths, exports: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if exports.is_empty() {
        return Ok(Vec::new());
    }
    let qmd = qmd_executable(paths).context("QMD executable was not found in a standard path")?;
    let mut missing = Vec::new();
    let mut chunk = Vec::new();
    let mut chunk_body_bytes = 0_u64;

    for export in exports {
        let lookup = prepare_qmd_export_lookup(export)?;
        let body_bytes = lookup.body.len() as u64;
        if !chunk.is_empty()
            && (chunk.len() >= QMD_EXACT_LOOKUP_MAX_EXPORTS
                || chunk_body_bytes.saturating_add(body_bytes) > QMD_EXACT_LOOKUP_MAX_BODY_BYTES)
        {
            collect_missing_qmd_exports(&qmd, &chunk, &mut missing)?;
            chunk.clear();
            chunk_body_bytes = 0;
        }
        chunk_body_bytes = chunk_body_bytes.saturating_add(body_bytes);
        chunk.push(lookup);
    }
    if !chunk.is_empty() {
        collect_missing_qmd_exports(&qmd, &chunk, &mut missing)?;
    }
    Ok(missing)
}

pub fn qmd_export_is_indexed(paths: &AgentPaths, export: &Path) -> Result<bool> {
    Ok(qmd_missing_exports(paths, &[export.to_path_buf()])?.is_empty())
}

fn prepare_qmd_export_lookup(export: &Path) -> Result<QmdExportLookup> {
    let metadata = fs::symlink_metadata(export)
        .with_context(|| format!("inspect Cursor history export {}", export.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "Cursor history export is not a regular file: {}",
            export.display()
        );
    }
    let name = export
        .file_name()
        .and_then(|name| name.to_str())
        .context("Cursor history export has no UTF-8 file name")?;
    let body = fs::read_to_string(export)
        .with_context(|| format!("read Cursor history export {}", export.display()))?;
    Ok(QmdExportLookup {
        export: export.to_path_buf(),
        virtual_path: format!("qmd://{QMD_CURSOR_COLLECTION}/{name}"),
        body,
    })
}

fn collect_missing_qmd_exports(
    qmd: &Path,
    chunk: &[QmdExportLookup],
    missing: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut exact_paths = chunk
        .iter()
        .map(|lookup| lookup.virtual_path.as_str())
        .collect::<Vec<_>>()
        .join(",");
    // The trailing comma intentionally selects QMD's exact-path list mode.
    exact_paths.push(',');
    let max_bytes = chunk
        .iter()
        .map(|lookup| lookup.body.len() as u64)
        .max()
        .unwrap_or_default()
        .saturating_add(4_096);
    let mut stdout = tempfile::tempfile().context("create QMD exact lookup output file")?;
    let child_stdout = stdout
        .try_clone()
        .context("clone QMD exact lookup output file")?;
    let output = Command::new(qmd)
        .args([
            "multi-get",
            &exact_paths,
            "--json",
            "--max-bytes",
            &max_bytes.to_string(),
        ])
        .stdout(Stdio::from(child_stdout))
        .output()
        .context("check exact Cursor history exports in QMD")?;
    if !output.status.success() {
        missing.extend(chunk.iter().map(|lookup| lookup.export.clone()));
        return Ok(());
    }
    stdout
        .seek(SeekFrom::Start(0))
        .context("rewind QMD exact lookup output")?;
    let mut stdout_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .context("read QMD exact lookup output")?;
    let indexed: Value = serde_json::from_slice(&stdout_bytes)
        .context("parse exact Cursor history exports returned by QMD")?;
    let Some(documents) = indexed.as_array() else {
        anyhow::bail!("qmd multi-get did not return a JSON array");
    };
    let documents_by_path = documents
        .iter()
        .filter_map(|document| {
            document
                .get("file")
                .and_then(Value::as_str)
                .map(|path| (path, document))
        })
        .collect::<BTreeMap<_, _>>();
    for lookup in chunk {
        let indexed_body = documents_by_path
            .get(lookup.virtual_path.as_str())
            .and_then(|document| document.get("body"))
            .and_then(Value::as_str);
        if !indexed_body.is_some_and(|body| {
            body.trim_end_matches(['\r', '\n']) == lookup.body.trim_end_matches(['\r', '\n'])
        }) {
            missing.push(lookup.export.clone());
        }
    }
    Ok(())
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
    let path = cursor_history_export_lock_path(paths)?;
    let mut waits = 0;
    loop {
        if let Some(lock) = create_cursor_history_export_lock(&path)? {
            return Ok(lock);
        }
        if remove_stale_lock_if_unchanged(&path)? {
            continue;
        }
        if waits >= CURSOR_EXPORT_LOCK_MAX_WAITS {
            anyhow::bail!("timed out waiting for Cursor history export lock");
        }
        waits += 1;
        std::thread::sleep(QMD_REFRESH_LOCK_WAIT);
    }
}

fn acquire_cursor_history_export_lock_for_mode(
    paths: &AgentPaths,
    mode: CursorHistoryLockMode,
) -> Result<Option<CursorHistoryExportLock>> {
    match mode {
        CursorHistoryLockMode::Wait => Ok(Some(acquire_cursor_history_export_lock(paths)?)),
        CursorHistoryLockMode::SkipIfBusy => try_acquire_cursor_history_export_lock(paths),
    }
}

fn try_acquire_cursor_history_export_lock(
    paths: &AgentPaths,
) -> Result<Option<CursorHistoryExportLock>> {
    create_cursor_history_export_lock(&cursor_history_export_lock_path(paths)?)
}

fn cursor_history_export_lock_path(paths: &AgentPaths) -> Result<PathBuf> {
    let state_dir = qmd_refresh_state_dir(paths);
    ensure_dir(&state_dir)?;
    Ok(state_dir.join("cursor-history-export.lock"))
}

fn create_cursor_history_export_lock(path: &Path) -> Result<Option<CursorHistoryExportLock>> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create Cursor history lock {}", path.display()))
        }
    };
    let token = format!("{}:{}", std::process::id(), unix_millis(SystemTime::now())?);
    writeln!(file, "{token}")
        .with_context(|| format!("write Cursor history lock {}", path.display()))?;
    file.sync_all()?;
    let snapshot = read_lock_snapshot(path)?.with_context(|| {
        format!(
            "Cursor history lock changed while acquiring {}",
            path.display()
        )
    })?;
    Ok(Some(CursorHistoryExportLock {
        path: path.to_path_buf(),
        snapshot,
    }))
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
    let age = lock_snapshot_age(&snapshot)?;
    let hard_expired = age.is_some_and(|age| age >= PERSISTENT_LOCK_HARD_MAX_AGE);
    let normally_stale = age.is_some_and(|age| age >= QMD_REFRESH_LOCK_STALE_AFTER);
    if !hard_expired && (lock_owner_is_active(&snapshot.token) || !normally_stale) {
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

fn lock_snapshot_age(snapshot: &LockSnapshot) -> Result<Option<Duration>> {
    let now = unix_millis(SystemTime::now())?;
    let token_age = snapshot
        .token
        .split_once(':')
        .and_then(|(_, timestamp)| timestamp.parse::<u64>().ok())
        .map(|timestamp| Duration::from_millis(now.saturating_sub(timestamp)));
    let modified_age = snapshot.identity.modified.elapsed().ok();
    Ok(match (token_age, modified_age) {
        (Some(token), Some(modified)) => Some(token.max(modified)),
        (Some(age), None) | (None, Some(age)) => Some(age),
        (None, None) => None,
    })
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

#[derive(Clone, Debug)]
struct PendingQmdMarker {
    path: PathBuf,
    snapshot: LockSnapshot,
}

#[derive(Debug, Default)]
struct PendingQmdWork {
    markers: Vec<PendingQmdMarker>,
    exports: Vec<PathBuf>,
    has_legacy_marker: bool,
}

#[derive(Debug)]
enum PendingQmdTarget {
    Legacy,
    Export(PathBuf),
}

fn write_qmd_pending_marker(paths: &AgentPaths, export: &Path) -> Result<()> {
    let timestamp = unix_millis(SystemTime::now())?;
    let export = export
        .canonicalize()
        .with_context(|| format!("resolve Cursor history export {}", export.display()))?;
    validate_pending_export_path(&export)?;
    let export_path = export.to_str().with_context(|| {
        format!(
            "Cursor history export path is not valid UTF-8: {}",
            export.display()
        )
    })?;
    let pending =
        checked_qmd_pending_dir(paths, true)?.context("QMD pending directory was not created")?;
    let path = pending.join(format!(
        "{QMD_PENDING_MARKER_PREFIX}{}{QMD_PENDING_MARKER_SUFFIX}",
        hash_bytes(export_path.as_bytes())
    ));
    let content = [
        serde_json::to_vec_pretty(&json!({
            "magic": QMD_PENDING_MARKER_MAGIC_V2,
            "timestampUnixMillis": timestamp,
            "exportPath": export_path,
        }))?,
        b"\n".to_vec(),
    ]
    .concat();
    write_atomic(&path, &content)?;
    set_private_file_mode(&path)
}

fn has_pending_qmd_work(paths: &AgentPaths, output_dir: &Path) -> Result<bool> {
    Ok(!pending_qmd_work(paths, output_dir)?.markers.is_empty())
}

fn pending_qmd_work(paths: &AgentPaths, output_dir: &Path) -> Result<PendingQmdWork> {
    let Some(pending) = checked_qmd_pending_dir(paths, false)? else {
        return Ok(PendingQmdWork::default());
    };
    let expected_output_dir = output_dir
        .canonicalize()
        .unwrap_or_else(|_| output_dir.to_path_buf());
    let mut work = PendingQmdWork::default();
    let mut exports = BTreeSet::new();
    for entry in fs::read_dir(&pending).with_context(|| format!("read {}", pending.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || !has_managed_qmd_pending_name(&entry.path()) {
            continue;
        }
        let path = entry.path();
        let Some(snapshot) = read_lock_snapshot(&path)? else {
            continue;
        };
        match parse_qmd_pending_target(&snapshot.token, &path)? {
            PendingQmdTarget::Legacy => {
                work.has_legacy_marker = true;
                work.markers.push(PendingQmdMarker { path, snapshot });
            }
            PendingQmdTarget::Export(export) => {
                let parent = export
                    .parent()
                    .context("managed QMD pending export has no parent directory")?;
                let parent = parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf());
                if parent == expected_output_dir {
                    exports.insert(export);
                    work.markers.push(PendingQmdMarker { path, snapshot });
                }
            }
        }
    }
    work.exports = exports.into_iter().collect();
    Ok(work)
}

fn clear_qmd_pending_markers(markers: &[PendingQmdMarker]) -> Result<()> {
    for marker in markers {
        remove_lock_if_snapshot_matches(&marker.path, &marker.snapshot)?;
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
        Ok(_) => {
            protect_qmd_pending_dir(&pending)?;
            Ok(Some(pending))
        }
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
            protect_qmd_pending_dir(&pending)?;
            Ok(Some(pending))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", pending.display())),
    }
}

fn protect_qmd_pending_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect QMD pending directory {}", path.display()))?;
    }
    Ok(())
}

fn is_managed_qmd_pending_marker(path: &Path) -> Result<bool> {
    if !has_managed_qmd_pending_name(path) {
        return Ok(false);
    }
    let Some(snapshot) = read_lock_snapshot(path)? else {
        return Ok(false);
    };
    parse_qmd_pending_target(&snapshot.token, path)?;
    Ok(true)
}

fn has_managed_qmd_pending_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with(QMD_PENDING_MARKER_PREFIX) && name.ends_with(QMD_PENDING_MARKER_SUFFIX)
        })
}

fn parse_qmd_pending_target(content: &str, marker: &Path) -> Result<PendingQmdTarget> {
    if content.lines().next() == Some(QMD_PENDING_MARKER_MAGIC_V1) {
        let mut lines = content.lines();
        lines.next();
        let timestamp = lines
            .next()
            .context("managed QMD pending marker has no timestamp")?;
        timestamp.parse::<u64>().with_context(|| {
            format!("parse QMD pending marker timestamp in {}", marker.display())
        })?;
        if lines.next().is_some() {
            anyhow::bail!(
                "managed QMD pending marker has extra data: {}",
                marker.display()
            );
        }
        return Ok(PendingQmdTarget::Legacy);
    }

    let value: Value = serde_json::from_str(content)
        .with_context(|| format!("parse managed QMD pending marker {}", marker.display()))?;
    if value.get("magic").and_then(Value::as_str) != Some(QMD_PENDING_MARKER_MAGIC_V2) {
        anyhow::bail!(
            "managed QMD pending marker is malformed: {}",
            marker.display()
        );
    }
    value
        .get("timestampUnixMillis")
        .and_then(Value::as_u64)
        .context("managed QMD pending marker has no timestamp")?;
    let export = value
        .get("exportPath")
        .and_then(Value::as_str)
        .context("managed QMD pending marker has no export path")?;
    let export = PathBuf::from(export);
    validate_pending_export_path(&export)?;
    Ok(PendingQmdTarget::Export(export))
}

fn validate_pending_export_path(export: &Path) -> Result<()> {
    let name = export
        .file_name()
        .and_then(|name| name.to_str())
        .context("managed QMD pending export has no UTF-8 file name")?;
    if !export.is_absolute() || !name.starts_with("cursor-") || !name.ends_with(".md") {
        anyhow::bail!(
            "managed QMD pending export path is unsafe: {}",
            export.display()
        );
    }
    Ok(())
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
    fn cursor_hook_edit_retries_and_preserves_a_concurrent_hook() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        fs::create_dir_all(&paths.cursor_home).unwrap();
        let hooks_path = paths.cursor_home.join("hooks.json");
        fs::write(&hooks_path, r#"{"version":1,"hooks":{"stop":[]}}"#).unwrap();
        let backup_root = temp.path().join("backups");
        let mut render_count = 0;

        let (changed, backup) = apply_cursor_hook_edit(&paths, &hooks_path, &backup_root, || {
            render_count += 1;
            if render_count == 1 {
                fs::write(
                    &hooks_path,
                    r#"{"version":1,"hooks":{"stop":[{"command":"concurrent"}]}}"#,
                )
                .unwrap();
            }
            render_cursor_hooks(&hooks_path, TEST_MANAGED_COMMAND)
        })
        .unwrap();

        assert!(changed);
        assert!(backup.is_some());
        assert_eq!(render_count, 2);
        let value: Value = serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
        assert_eq!(value["hooks"]["stop"][0]["command"], "concurrent");
        assert_eq!(value["hooks"]["stop"][1]["command"], TEST_MANAGED_COMMAND);
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
        let sessions = default_cursor_history_output_dir(&paths);
        assert!(sessions.join("cursor-chat.md").exists());
        assert!(sessions.join("cursor-chat-subagent-worker.md").exists());
        let main_export = fs::read_to_string(sessions.join("cursor-chat.md")).unwrap();
        assert!(main_export.contains("model: \"cursor-model\""));
        assert!(main_export.contains("workspace_roots: [\"/example\"]"));
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 2);
    }

    #[test]
    fn cursor_history_sweep_skips_only_unreadable_transcripts() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let valid = write_test_transcript(&paths);
        let unreadable = paths
            .cursor_home
            .join("projects/example/agent-transcripts/new-format/new-format.jsonl");
        fs::create_dir_all(unreadable.parent().unwrap()).unwrap();
        fs::write(&unreadable, "{\"version\":999,\"newCursorSchema\":true}\n").unwrap();

        let report = sweep_cursor_history_report_to(
            &paths,
            &default_cursor_history_output_dir(&paths),
            false,
        )
        .unwrap();

        assert_eq!(report.exported, 1);
        assert_eq!(report.unreadable, vec![unreadable.clone()]);
        assert!(default_cursor_history_output_dir(&paths)
            .join("cursor-session.md")
            .exists());
        assert_eq!(cursor_history_unreadable_count(&paths).unwrap(), 1);
        assert_eq!(
            cursor_history_coverage(&paths).unwrap().unreadable,
            vec![unreadable.canonicalize().unwrap()]
        );
        assert!(valid.exists());
    }

    #[test]
    fn hook_export_skips_a_busy_lock_and_sweep_recovers_the_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let output_dir = temp.path().join("sessions");
        let export_lock = acquire_cursor_history_export_lock(&paths).unwrap();

        let started = std::time::Instant::now();
        let skipped = export_cursor_history_with_lock_mode(
            &paths,
            &test_hook(&transcript),
            Some(output_dir.clone()),
            false,
            CursorHistoryLockMode::SkipIfBusy,
        )
        .unwrap();

        assert_eq!(skipped, None);
        assert!(started.elapsed() < QMD_REFRESH_LOCK_WAIT);
        assert!(!output_dir.join("cursor-session.md").exists());
        drop(export_lock);

        assert_eq!(
            sweep_cursor_history_to(&paths, &output_dir, false).unwrap(),
            1
        );
        assert!(output_dir.join("cursor-session.md").exists());
    }

    #[test]
    fn manual_export_keeps_waiting_for_a_busy_lock() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let export_lock = acquire_cursor_history_export_lock(&paths).unwrap();
        let export_paths = paths.clone();
        let export = std::thread::spawn(move || {
            export_cursor_history_with_lock_mode(
                &export_paths,
                &test_hook(&transcript),
                None,
                false,
                CursorHistoryLockMode::Wait,
            )
        });

        std::thread::sleep(Duration::from_millis(50));
        assert!(!export.is_finished());
        drop(export_lock);

        assert!(export.join().unwrap().unwrap().is_some());
    }

    #[test]
    fn hook_without_a_transcript_does_not_create_lock_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());

        assert_eq!(
            export_cursor_history_with_lock_mode(
                &paths,
                &json!({}),
                None,
                false,
                CursorHistoryLockMode::SkipIfBusy,
            )
            .unwrap(),
            None
        );
        assert!(!qmd_refresh_state_dir(&paths).exists());
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_cursor_history_does_not_refresh_qmd_again() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);
        let hook = test_hook(&transcript);

        export_cursor_history(&paths, &hook, None, true).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "update\nembed\n");

        export_cursor_history(&paths, &hook, None, true).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "update\nembed\n");
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
        let output = default_cursor_history_output_dir(&paths).join("cursor-session.md");
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
        assert!(!default_cursor_history_output_dir(&paths)
            .join("cursor-another-chat.md")
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
    fn glob_matcher_preserves_single_and_double_star_semantics() {
        for (pattern, candidate) in [
            ("*.md", "cursor-chat.md"),
            ("cursor-?.md", "cursor-a.md"),
            ("**/cursor-*.md", "cursor-chat.md"),
            ("**/cursor-*.md", "nested/deep/cursor-chat.md"),
            ("notes/**/cursor-*.md", "notes/cursor-chat.md"),
            ("notes/**/cursor-*.md", "notes/year/month/cursor-chat.md"),
        ] {
            assert!(glob_matches(pattern.as_bytes(), candidate.as_bytes()));
        }
        for (pattern, candidate) in [
            ("*.md", "nested/cursor-chat.md"),
            ("cursor-?.md", "cursor-chat.md"),
            ("notes/**/cursor-*.md", "other/cursor-chat.md"),
        ] {
            assert!(!glob_matches(pattern.as_bytes(), candidate.as_bytes()));
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

        install_test_qmd_with_config(&paths, &log, None, "codex-*.md", "yes (default)", true, 0);
        let error = qmd_health(&paths).unwrap_err();
        assert!(error.to_string().contains("does not cover cursor-*.md"));

        install_test_qmd_with_config(&paths, &log, None, "**/*.md", "no", true, 0);
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
    fn qmd_exact_lookups_use_bounded_batches() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let output_dir = default_cursor_history_output_dir(&paths);
        fs::create_dir_all(&output_dir).unwrap();
        let exports = (0..130)
            .map(|index| {
                let export = output_dir.join(format!("cursor-batch-{index:03}.md"));
                fs::write(&export, format!("chat {index}\n")).unwrap();
                export
            })
            .collect::<Vec<_>>();
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);

        assert!(qmd_missing_exports(&paths, &exports).unwrap().is_empty());

        let commands = qmd_test_commands(&log);
        assert_eq!(
            commands
                .iter()
                .map(|command| command.split_once('\t').unwrap().0)
                .collect::<Vec<_>>(),
            vec!["multi-get", "multi-get", "multi-get"]
        );
        assert_eq!(
            commands
                .iter()
                .map(|command| {
                    command
                        .split_once('\t')
                        .unwrap()
                        .1
                        .trim_end_matches(',')
                        .split(',')
                        .count()
                })
                .collect::<Vec<_>>(),
            vec![64, 64, 2]
        );
    }

    #[cfg(unix)]
    #[test]
    fn qmd_large_single_line_uses_file_stdout_and_detects_late_staleness() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let output_dir = default_cursor_history_output_dir(&paths);
        fs::create_dir_all(&output_dir).unwrap();
        let export = output_dir.join("cursor-large.md");
        let body = format!("{}\n", "x".repeat(120_000));
        fs::write(&export, &body).unwrap();
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);
        fs::write(output_dir.join(".qmd-truncate-pipe"), "").unwrap();

        assert!(qmd_missing_exports(&paths, std::slice::from_ref(&export))
            .unwrap()
            .is_empty());
        assert_eq!(
            qmd_test_commands(&log)
                .iter()
                .map(|command| command.split_once('\t').unwrap().0)
                .collect::<Vec<_>>(),
            vec!["multi-get"]
        );

        let mut stale_body = body.into_bytes();
        stale_body[80_000] = b'y';
        fs::write(output_dir.join(".qmd-indexed-cursor-large.md"), stale_body).unwrap();
        fs::write(log.with_extension("commands.log"), "").unwrap();
        assert_eq!(
            qmd_missing_exports(&paths, std::slice::from_ref(&export)).unwrap(),
            vec![export]
        );
    }

    #[cfg(unix)]
    #[test]
    fn pending_refresh_verifies_only_pending_exports() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let output_dir = default_cursor_history_output_dir(&paths);
        fs::create_dir_all(&output_dir).unwrap();
        for index in 0..100 {
            fs::write(
                output_dir.join(format!("cursor-existing-{index:03}.md")),
                format!("existing chat {index}\n"),
            )
            .unwrap();
        }
        let pending = output_dir.join("cursor-pending.md");
        fs::write(&pending, "pending chat\n").unwrap();
        write_qmd_pending_marker(&paths, &pending).unwrap();
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);

        assert!(refresh_pending_qmd_index_for_output(&paths, &output_dir, true).unwrap());

        let commands = qmd_test_commands(&log);
        assert_eq!(
            commands
                .iter()
                .map(|command| command.split_once('\t').unwrap().0)
                .collect::<Vec<_>>(),
            vec!["update", "embed", "collection", "status", "multi-get"]
        );
        let exact_paths = commands.last().unwrap().split_once('\t').unwrap().1;
        assert_eq!(
            exact_paths,
            format!("qmd://{QMD_CURSOR_COLLECTION}/cursor-pending.md,")
        );
        assert_eq!(qmd_pending_exports(&paths).unwrap(), 0);
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
        let output_dir = default_cursor_history_output_dir(&paths);
        fs::create_dir_all(&output_dir).unwrap();
        let export = output_dir.join("cursor-same-chat.md");
        fs::write(&export, "test\n").unwrap();
        let pending = qmd_pending_dir(&paths);
        fs::create_dir_all(&pending).unwrap();
        let unrelated = pending.join("personal-note.txt");
        fs::write(&unrelated, "keep me\n").unwrap();
        write_qmd_pending_marker(&paths, &export).unwrap();
        let work = pending_qmd_work(&paths, &output_dir).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        write_qmd_pending_marker(&paths, &export).unwrap();

        assert_eq!(qmd_pending_exports(&paths).unwrap(), 1);
        clear_qmd_pending_markers(&work.markers).unwrap();

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

        let error =
            pending_qmd_work(&paths, &default_cursor_history_output_dir(&paths)).unwrap_err();

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
        let export = export_cursor_history(&paths, &test_hook(&transcript), None, false)
            .unwrap()
            .unwrap();
        write_qmd_pending_marker(&paths, &export).unwrap();
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
    fn refresh_ignores_unrelated_global_pending_embeddings() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd_with_config(&paths, &log, None, "**/*.md", "yes (default)", true, 7);
        let export = export_cursor_history(&paths, &test_hook(&transcript), None, false)
            .unwrap()
            .unwrap();
        write_qmd_pending_marker(&paths, &export).unwrap();

        assert!(refresh_qmd_index(&paths, true).unwrap());
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
        install_test_qmd_with_config(&paths, &log, None, "**/*.md", "yes (default)", false, 0);
        let export = export_cursor_history(&paths, &test_hook(&transcript), None, false)
            .unwrap()
            .unwrap();
        write_qmd_pending_marker(&paths, &export).unwrap();

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
    fn hard_expired_lock_is_removed_even_when_its_pid_is_live() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("refresh.lock");
        fs::write(&path, format!("{}:0\n", std::process::id())).unwrap();

        let snapshot = read_lock_snapshot(&path).unwrap().unwrap();
        assert!(lock_owner_is_active(&snapshot.token));
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
        install_test_qmd_with_config(paths, log, fail_on, "**/*.md", "yes (default)", true, 0);
    }

    #[cfg(unix)]
    fn install_test_qmd_with_config(
        paths: &AgentPaths,
        log: &Path,
        fail_on: Option<&str>,
        pattern: &str,
        include: &str,
        retrieve_exports: bool,
        pending_embeddings: usize,
    ) {
        use std::os::unix::fs::PermissionsExt;

        let qmd = paths.home.join(".local/bin/qmd");
        let sessions = default_cursor_history_output_dir(paths);
        let export_lock = qmd_refresh_state_dir(paths).join("cursor-history-export.lock");
        let command_log = log.with_extension("commands.log");
        fs::create_dir_all(qmd.parent().unwrap()).unwrap();
        let failure = fail_on
            .map(|subcommand| format!("[ \"$1\" = \"{subcommand}\" ] && exit 9\n"))
            .unwrap_or_default();
        let multi_get_result = if retrieve_exports {
            format!(
                concat!(
                    "paths=${{2%,}}\n",
                    "old_ifs=$IFS\n",
                    "IFS=,\n",
                    "set -f\n",
                    "set -- $paths\n",
                    "set +f\n",
                    "IFS=$old_ifs\n",
                    "printf '['\n",
                    "separator=''\n",
                    "for virtual in \"$@\"; do\n",
                    "  name=${{virtual##*/}}\n",
                    "  target={}/\"$name\"\n",
                    "  indexed={}/.qmd-indexed-\"$name\"\n",
                    "  [ -f \"$indexed\" ] && target=$indexed\n",
                    "  [ -f \"$target\" ] || continue\n",
                    "  printf '%s{{\"file\":\"qmd://{}/%s\",\"title\":\"test\",\"body\":\"' \"$separator\" \"$name\"\n",
                    "  sed -e 's/\\\\/\\\\\\\\/g' -e 's/\"/\\\\\"/g' -e 's/$/\\\\n/' \"$target\" | tr -d '\\n'\n",
                    "  printf '\"}}'\n",
                    "  separator=','\n",
                    "done\n",
                    "printf ']\\n'\n"
                ),
                shell_quote(&sessions.to_string_lossy()),
                shell_quote(&sessions.to_string_lossy()),
                QMD_CURSOR_COLLECTION
            )
        } else {
            "printf '[]\\n'\n".to_string()
        };
        fs::write(
            &qmd,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "printf '%s\\t%s\\n' \"$1\" \"$2\" >> {}\n",
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
                    "  printf '%s\\n' '  Pending:  {} need embedding'\n",
                    "  exit 0\n",
                    "  ;;\n",
                    "multi-get)\n",
                    "  if [ -e {}/.qmd-truncate-pipe ] && [ -p /dev/stdout ]; then printf '['; exit 0; fi\n",
                    "  {multi_get_result}",
                    "  ;;\n",
                    "*) exit 9 ;;\n",
                    "esac\n"
                ),
                shell_quote(&command_log.to_string_lossy()),
                shell_quote(&log.to_string_lossy()),
                shell_quote(&export_lock.to_string_lossy()),
                sessions.display(),
                pattern,
                include,
                pending_embeddings,
                sessions.display(),
                failure = failure,
                multi_get_result = multi_get_result,
            ),
        )
        .unwrap();
        fs::set_permissions(&qmd, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn qmd_test_commands(log: &Path) -> Vec<String> {
        fs::read_to_string(log.with_extension("commands.log"))
            .unwrap()
            .lines()
            .map(ToString::to_string)
            .collect()
    }
}

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    adapters::{AgentKind, AgentPaths},
    agent_skill::{
        install_agent_skills, AgentSkillInstallAction, AgentSkillInstallOptions,
        AgentSkillInstallSetReport,
    },
    apply::{apply_pack, diff_pack, verify_pack, ApplyOptions, Change, ChangeAction},
    config::{load_config, render_config, CanonicalSource, Config, McpMode},
    cursor_history::{
        cursor_history_coverage_at, cursor_history_unreadable_count,
        default_cursor_history_output_dir, ensure_qmd_collection,
        install_cursor_history_hook_with_refresh, qmd_executable, qmd_health, qmd_missing_exports,
        qmd_pending_exports, qmd_refresh_last_success, refresh_pending_qmd_index_for_output,
        refresh_qmd_index_for_output, remove_cursor_history_hook, sweep_cursor_history_report_to,
    },
    fsx::{
        ensure_dir, hash_bytes, hash_path, read_to_string_if_exists, remove_target_if_unchanged,
        replace_file_with_backup_if_unchanged, restore_backup_atomically_if_unchanged,
        write_atomic,
    },
    mcp::{
        discover_claude_mcp, discover_codex_mcp, discover_cursor_mcp, ensure_cursor_mcp_write_safe,
    },
    pack::{export_pack, ExportOptions, SourceSelection},
};

const RUN_STATE_VERSION: u32 = 1;
const PERSISTENT_LOCK_HARD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupOptions {
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupReport {
    pub dry_run: bool,
    pub config_path: PathBuf,
    pub config_changed: bool,
    pub config_backup: Option<PathBuf>,
    pub skill: AgentSkillInstallSetReport,
    pub policy_summary: String,
}

impl SetupReport {
    pub fn to_text(&self) -> String {
        let mut out = if self.config_changed && self.dry_run {
            format!(
                "Dry run. Write managed config -> {}\n",
                self.config_path.display()
            )
        } else if self.config_changed {
            format!("Saved managed config -> {}\n", self.config_path.display())
        } else {
            format!(
                "Unchanged managed config -> {}\n",
                self.config_path.display()
            )
        };
        out.push_str(&self.policy_summary);
        if let Some(backup) = &self.config_backup {
            out.push_str(&format!("Config backup: {}\n", backup.display()));
        }
        out.push_str(&self.skill.to_text());
        if self.dry_run {
            out.push_str("No files written. Run the same setup command with --yes to save it.\n");
        } else {
            out.push_str("Setup is ready. Run `agent-sync sync` to preview the first sync.\n");
        }
        out
    }
}

pub fn setup_managed(
    paths: &AgentPaths,
    config_path: &Path,
    config: &Config,
    options: SetupOptions,
) -> Result<SetupReport> {
    let _lock = if options.dry_run {
        None
    } else {
        Some(SyncLock::acquire(paths)?)
    };
    config.validate()?;
    preflight(paths, config)?;
    ensure_regular_or_missing(config_path, "managed config")?;
    let rendered = render_config(config)?;
    let config_changed = read_to_string_if_exists(config_path)?.as_deref() != Some(&rendered);
    let skill_preview = install_agent_skills(paths, AgentSkillInstallOptions { dry_run: true })?;

    if options.dry_run {
        return Ok(SetupReport {
            dry_run: true,
            config_path: config_path.to_path_buf(),
            config_changed,
            config_backup: None,
            skill: skill_preview,
            policy_summary: format_config_summary(config),
        });
    }

    let original_config = if config_changed {
        match fs::read(config_path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read managed config {}", config_path.display()));
            }
        }
    } else {
        None
    };
    let backup_root = paths
        .home
        .join(".agent-sync/backups")
        .join(Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string())
        .join("setup");
    let config_backup = if config_changed {
        replace_file_with_backup_if_unchanged(
            &backup_root,
            &paths.home,
            config_path,
            original_config.as_deref(),
            rendered.as_bytes(),
        )?
    } else {
        None
    };
    let skill = match install_agent_skills(paths, AgentSkillInstallOptions { dry_run: false }) {
        Ok(report) => report,
        Err(error) => {
            let installed_sha256 = hash_bytes(rendered.as_bytes());
            let rollback = match (config_changed, original_config.as_ref()) {
                (true, Some(_)) => restore_backup_atomically_if_unchanged(
                    config_backup
                        .as_deref()
                        .context("updated config backup was not recorded")?,
                    config_path,
                    &installed_sha256,
                ),
                (true, None) => remove_target_if_unchanged(config_path, &installed_sha256),
                (false, _) => Ok(()),
            };
            if let Err(rollback_error) = rollback {
                return Err(anyhow::anyhow!(
                    "install bundled skill failed: {error:#}; config rollback also failed: {rollback_error:#}"
                ));
            }
            return Err(error).context("install bundled skill; config was rolled back");
        }
    };

    Ok(SetupReport {
        dry_run: false,
        config_path: config_path.to_path_buf(),
        config_changed,
        config_backup,
        skill,
        policy_summary: format_config_summary(config),
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeCounts {
    pub add: usize,
    pub update: usize,
    pub unchanged: usize,
    pub preserved: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorHistoryMode {
    Disabled,
    ExportOnly,
    Qmd,
}

impl CursorHistoryMode {
    fn from_config(config: &Config) -> Self {
        match (
            config.cursor_history.enabled,
            config.cursor_history.refresh_qmd,
        ) {
            (false, _) => Self::Disabled,
            (true, false) => Self::ExportOnly,
            (true, true) => Self::Qmd,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::ExportOnly => "enabled without QMD refresh",
            Self::Qmd => "enabled with QMD refresh",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusNextAction {
    None,
    PreviewSync,
    RunDoctor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusLastSuccess {
    pub finished_at: DateTime<Utc>,
    pub result: RunResult,
    pub agent_sync_version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedStatus {
    pub source: CanonicalSource,
    pub targets: Vec<AgentKind>,
    pub cursor_history: CursorHistoryMode,
    pub drift: ChangeCounts,
    pub last_success: Option<StatusLastSuccess>,
    pub healthy: bool,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub next_action: StatusNextAction,
}

impl ManagedStatus {
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "Managed route: {} -> {}\nCursor history: {}\nDrift: {} add, {} update, {} preserved, {} unchanged\n",
            self.source.agent_kind(),
            self.targets
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
            self.cursor_history.label(),
            self.drift.add,
            self.drift.update,
            self.drift.preserved,
            self.drift.unchanged
        );
        match &self.last_success {
            Some(record) => out.push_str(&format!(
                "Last successful sync: {} ({:?}, agent-sync {})\n",
                record.finished_at.to_rfc3339(),
                record.result,
                record.agent_sync_version
            )),
            None => out.push_str("Last successful sync: never\n"),
        }
        out.push_str(
            if !self.healthy && self.next_action == StatusNextAction::PreviewSync {
                "Health: first sync or repair is ready to preview\n"
            } else if self.healthy && self.warnings.is_empty() {
                "Health: healthy\n"
            } else if self.healthy {
                "Health: healthy with warnings\n"
            } else {
                "Health: needs attention; run `agent-sync doctor`\n"
            },
        );
        for warning in &self.warnings {
            out.push_str(&format!("Warning: {warning}\n"));
        }
        out.push_str(match self.next_action {
            StatusNextAction::PreviewSync => {
                "Next action: run `agent-sync sync` to preview the repair.\n"
            }
            StatusNextAction::RunDoctor => {
                "Next action: run `agent-sync doctor` for the failing checks.\n"
            }
            StatusNextAction::None => "Next action: none.\n",
        });
        out
    }
}

impl ChangeCounts {
    fn from_changes(changes: &[Change]) -> Self {
        let mut counts = Self::default();
        for change in changes {
            match change.action {
                ChangeAction::Add => counts.add += 1,
                ChangeAction::Update | ChangeAction::ManagedUpdate => counts.update += 1,
                ChangeAction::Unchanged => counts.unchanged += 1,
                ChangeAction::Skip => counts.preserved += 1,
            }
        }
        counts
    }

    fn has_writes(self) -> bool {
        self.add > 0 || self.update > 0
    }
}

fn plain_update_count(changes: &[Change]) -> usize {
    changes
        .iter()
        .filter(|change| change.action == ChangeAction::Update)
        .count()
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunResult {
    Preview,
    Healthy,
    Changed,
    Attention,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunRecord {
    pub version: u32,
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub result: RunResult,
    pub applied: bool,
    #[serde(default)]
    pub apply_started: bool,
    #[serde(default)]
    pub executable: PathBuf,
    #[serde(default)]
    pub agent_sync_version: String,
    #[serde(default)]
    pub executable_sha256: String,
    #[serde(default)]
    pub config_sha256: String,
    #[serde(default)]
    pub manifest_sha256: Option<String>,
    pub source: CanonicalSource,
    pub targets: Vec<AgentKind>,
    pub before: ChangeCounts,
    pub after: ChangeCounts,
    pub preserved: Vec<String>,
    pub verification_ok: bool,
    #[serde(default)]
    pub bundled_skill_changed: bool,
    #[serde(default)]
    pub cursor_history_checked: usize,
    #[serde(default)]
    pub cursor_history_unreadable: usize,
    pub history_hook_changed: bool,
    pub qmd_refreshed: bool,
    pub backup_root: Option<PathBuf>,
    #[serde(default)]
    pub failed_phase: Option<String>,
    pub error: Option<String>,
}

#[derive(Default)]
struct SyncProgress {
    phase: &'static str,
    before: ChangeCounts,
    after: ChangeCounts,
    preserved: Vec<String>,
    manifest_sha256: Option<String>,
    apply_started: bool,
    apply_completed: bool,
    verification_ok: bool,
    bundled_skill_changed: bool,
    cursor_history_checked: usize,
    cursor_history_unreadable: usize,
    history_hook_changed: bool,
    qmd_refreshed: bool,
    backup_root: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub automation: bool,
}

#[derive(Clone, Debug)]
pub struct SyncReport {
    pub record: RunRecord,
    pub changes: Vec<Change>,
    pub new_preserved: Vec<String>,
    pub automation: bool,
    pub updates_allowed: bool,
    pub skill_action: AgentSkillInstallAction,
    pub qmd_refresh_enabled: bool,
    pub cursor_history_sweep_enabled: bool,
}

impl SyncReport {
    pub fn to_text(&self) -> String {
        if self.automation
            && self.record.result == RunResult::Healthy
            && !self.record.before.has_writes()
            && self.new_preserved.is_empty()
        {
            return "DONT_NOTIFY\n".to_string();
        }

        let route = format!(
            "{} -> {}",
            self.record.source.agent_kind(),
            self.record
                .targets
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut out = format!(
            "Agent sync {:?}: {route}\nAdd: {}  Update: {}  Preserved: {}  Unchanged: {}\n",
            self.record.result,
            self.record.before.add,
            self.record.before.update,
            self.record.before.preserved,
            self.record.before.unchanged
        );
        if self.record.applied {
            out.push_str("Verification passed.\n");
        } else {
            out.push_str("Preview only. No agent files were changed.\n");
            if self.record.before.has_writes()
                || self.skill_action != AgentSkillInstallAction::Unchanged
                || self.record.history_hook_changed
                || self.qmd_refresh_enabled
                || self.cursor_history_sweep_enabled
            {
                out.push_str("Run `agent-sync sync --yes` to apply this plan.\n");
            }
        }
        if self.record.cursor_history_unreadable > 0 {
            out.push_str(&format!(
                "Skipped {} unreadable Cursor transcript(s); run `agent-sync doctor` for details.\n",
                self.record.cursor_history_unreadable
            ));
        }
        let writable = self
            .changes
            .iter()
            .filter(|change| {
                matches!(
                    change.action,
                    ChangeAction::Add | ChangeAction::Update | ChangeAction::ManagedUpdate
                )
            })
            .collect::<Vec<_>>();
        if !writable.is_empty() {
            out.push_str("Planned changes:\n");
            for change in writable {
                out.push_str(&format!(
                    "- {:?} {} {}\n",
                    change.action, change.target, change.resource
                ));
            }
        }
        if plain_update_count(&self.changes) > 0 && !self.updates_allowed {
            out.push_str(
                "This plan is blocked because target replacements are disabled. Keep the target-owned content or explicitly enable updates during setup.\n",
            );
        }
        if !self.record.preserved.is_empty() {
            out.push_str("Preserved target-owned resources:\n");
            for resource in &self.record.preserved {
                out.push_str(&format!("- {resource}\n"));
            }
        }
        if self.skill_action != AgentSkillInstallAction::Unchanged {
            if self.record.bundled_skill_changed {
                out.push_str("The natural-language agent-sync skill was maintained.\n");
            } else {
                out.push_str(
                    "Planned maintenance: refresh the natural-language agent-sync skill.\n",
                );
            }
        }
        if self.record.history_hook_changed {
            if self.record.applied {
                out.push_str("The managed Cursor history hook was maintained.\n");
            } else {
                out.push_str("Planned maintenance: reconcile the managed Cursor history hook.\n");
            }
        }
        if !self.record.applied && self.qmd_refresh_enabled {
            out.push_str("Planned maintenance: refresh the QMD index and embeddings.\n");
        }
        if !self.record.applied && self.cursor_history_sweep_enabled {
            out.push_str("Planned maintenance: sweep missed Cursor transcripts.\n");
        } else if self.record.applied && self.record.cursor_history_checked > 0 {
            out.push_str(&format!(
                "Checked {} Cursor transcript(s) for history export.\n",
                self.record.cursor_history_checked
            ));
        }
        if self.record.qmd_refreshed {
            out.push_str("QMD index and embeddings refreshed.\n");
        }
        out
    }
}

pub fn sync_managed(
    paths: &AgentPaths,
    config_path: &Path,
    executable: &Path,
    options: SyncOptions,
) -> Result<SyncReport> {
    let _lock = if options.dry_run {
        None
    } else {
        Some(SyncLock::acquire(paths)?)
    };
    if !options.dry_run {
        preflight_run_state(paths)?;
    }
    let config = load_config(config_path)?;
    let executable_sha256 = hash_path(executable)?;
    let config_sha256 = hash_path(config_path)?;
    let previous = load_last_success(paths)?;
    let started_at = Utc::now();
    let run_id = format!(
        "{}-{}",
        started_at.format("%Y%m%dT%H%M%S%.6fZ"),
        std::process::id()
    );
    let mut progress = SyncProgress {
        phase: "preflight",
        ..SyncProgress::default()
    };

    match sync_inner(
        paths,
        &config,
        executable,
        &executable_sha256,
        &config_sha256,
        &mut progress,
        options,
    ) {
        Ok(mut report) => {
            let previous_preserved = previous
                .as_ref()
                .map(|record| record.preserved.iter().cloned().collect::<BTreeSet<_>>())
                .unwrap_or_default();
            report.new_preserved = report
                .record
                .preserved
                .iter()
                .filter(|resource| !previous_preserved.contains(*resource))
                .cloned()
                .collect();
            if report.record.applied
                && !report.new_preserved.is_empty()
                && report.record.result == RunResult::Healthy
            {
                report.record.result = RunResult::Attention;
            }
            report.record.run_id = run_id;
            report.record.started_at = started_at;
            report.record.finished_at = Utc::now();
            if report.record.applied {
                write_run_record(paths, &report.record, true)?;
            }
            Ok(report)
        }
        Err(error) => {
            let record = RunRecord {
                version: RUN_STATE_VERSION,
                run_id,
                started_at,
                finished_at: Utc::now(),
                result: RunResult::Failed,
                applied: progress.apply_completed,
                apply_started: progress.apply_started,
                executable: executable.to_path_buf(),
                agent_sync_version: env!("CARGO_PKG_VERSION").to_string(),
                executable_sha256,
                config_sha256,
                manifest_sha256: progress.manifest_sha256,
                source: config.source,
                targets: config.targets.clone(),
                before: progress.before,
                after: progress.after,
                preserved: progress.preserved,
                verification_ok: progress.verification_ok,
                bundled_skill_changed: progress.bundled_skill_changed,
                cursor_history_checked: progress.cursor_history_checked,
                cursor_history_unreadable: progress.cursor_history_unreadable,
                history_hook_changed: progress.history_hook_changed,
                qmd_refreshed: progress.qmd_refreshed,
                backup_root: progress.backup_root,
                failed_phase: Some(progress.phase.to_string()),
                error: Some(format!("{error:#}")),
            };
            match write_run_record(paths, &record, false) {
                Ok(()) => Err(error),
                Err(state_error) => Err(anyhow::anyhow!(
                    "sync failed: {error:#}; recording the failed run also failed: {state_error:#}"
                )),
            }
        }
    }
}

fn sync_inner(
    paths: &AgentPaths,
    config: &Config,
    executable: &Path,
    executable_sha256: &str,
    config_sha256: &str,
    progress: &mut SyncProgress,
    options: SyncOptions,
) -> Result<SyncReport> {
    progress.phase = "preflight";
    preflight(paths, config)?;
    progress.phase = "export";
    let temp = if options.dry_run {
        tempfile::Builder::new()
            .prefix("agent-sync-preview-")
            .tempdir()
            .context("create private preview pack")?
    } else {
        let temp_root = paths.home.join(".agent-sync/tmp");
        ensure_dir(&temp_root)?;
        tempfile::Builder::new()
            .prefix("sync-")
            .tempdir_in(&temp_root)
            .with_context(|| format!("create private sync pack in {}", temp_root.display()))?
    };
    let pack = temp.path();
    export_pack(paths, pack, export_options(config))?;
    let manifest_sha256 = hash_path(&pack.join("agent-sync.manifest.json"))?;
    progress.manifest_sha256 = Some(manifest_sha256.clone());
    progress.phase = "diff";
    let changes = diff_pack(paths, pack, &config.targets)?;
    let before = ChangeCounts::from_changes(&changes);
    progress.before = before;
    let preserved = changes
        .iter()
        .filter(|change| change.action == ChangeAction::Skip)
        .map(|change| format!("{} {}", change.target, change.resource))
        .collect::<Vec<_>>();
    progress.preserved = preserved.clone();
    progress.phase = "skill-preflight";
    let skill_preview = install_agent_skills(paths, AgentSkillInstallOptions { dry_run: true })?;
    let history_output_dir = config
        .cursor_history
        .enabled
        .then(|| cursor_history_output_dir(paths, config))
        .transpose()?;
    progress.phase = "history-preflight";
    let history_hook_preview = if config.cursor_history.enabled {
        let output_dir = history_output_dir
            .as_deref()
            .context("Cursor history output directory was not resolved")?;
        let hook = install_cursor_history_hook_with_refresh(
            paths,
            executable,
            output_dir,
            true,
            config.cursor_history.refresh_qmd,
        )?;
        if config.cursor_history.refresh_qmd && !ensure_qmd_collection(paths, true)? {
            verify_qmd_health(paths, output_dir)?;
        }
        hook.changed
    } else {
        remove_cursor_history_hook(paths, true)?.changed
    };

    if options.dry_run {
        let updates_allowed = config.allow_updates || plain_update_count(&changes) == 0;
        return Ok(SyncReport {
            record: RunRecord {
                version: RUN_STATE_VERSION,
                run_id: String::new(),
                started_at: Utc::now(),
                finished_at: Utc::now(),
                result: RunResult::Preview,
                applied: false,
                apply_started: false,
                executable: executable.to_path_buf(),
                agent_sync_version: env!("CARGO_PKG_VERSION").to_string(),
                executable_sha256: executable_sha256.to_string(),
                config_sha256: config_sha256.to_string(),
                manifest_sha256: Some(manifest_sha256),
                source: config.source,
                targets: config.targets.clone(),
                before,
                after: before,
                preserved,
                verification_ok: false,
                bundled_skill_changed: false,
                cursor_history_checked: 0,
                cursor_history_unreadable: 0,
                history_hook_changed: history_hook_preview,
                qmd_refreshed: false,
                backup_root: None,
                failed_phase: None,
                error: None,
            },
            changes,
            new_preserved: Vec::new(),
            automation: options.automation,
            updates_allowed,
            skill_action: skill_preview.action(),
            qmd_refresh_enabled: config.cursor_history.enabled && config.cursor_history.refresh_qmd,
            cursor_history_sweep_enabled: config.cursor_history.enabled,
        });
    }

    let blocked_updates = plain_update_count(&changes);
    if blocked_updates > 0 && !config.allow_updates {
        progress.phase = "policy";
        bail!(
            "sync is blocked because {} update(s) require replacing target content; review `agent-sync sync` and enable allow_updates explicitly only if replacement is intended",
            blocked_updates
        );
    }

    progress.phase = "skill-reconcile";
    let skill = install_agent_skills(paths, AgentSkillInstallOptions { dry_run: false })?;
    let bundled_skill_changed = skill.action() != AgentSkillInstallAction::Unchanged;
    progress.bundled_skill_changed = bundled_skill_changed;

    let mut qmd_refreshed = false;
    let mut cursor_history_checked = 0;
    progress.phase = "history-preflight";
    if config.cursor_history.enabled {
        let output_dir = history_output_dir
            .as_deref()
            .context("Cursor history output directory was not resolved")?;
        install_cursor_history_hook_with_refresh(
            paths,
            executable,
            output_dir,
            true,
            config.cursor_history.refresh_qmd,
        )?;
        progress.phase = "history-sweep";
        let sweep =
            sweep_cursor_history_report_to(paths, output_dir, config.cursor_history.refresh_qmd)?;
        cursor_history_checked = sweep.exported;
        progress.cursor_history_checked = sweep.exported;
        progress.cursor_history_unreadable = sweep.unreadable.len();
        if config.cursor_history.refresh_qmd {
            ensure_qmd_collection(paths, false)?;
            progress.phase = "qmd-refresh";
            qmd_refreshed = if progress.cursor_history_unreadable > 0 {
                refresh_pending_qmd_index_for_output(paths, output_dir, true)?
            } else {
                refresh_qmd_index_for_output(paths, output_dir, true)?
            };
            progress.qmd_refreshed = qmd_refreshed;
        }
    } else {
        remove_cursor_history_hook(paths, true)?;
    }

    progress.phase = "apply";
    progress.apply_started = true;
    let apply_backup_root = before.has_writes().then(|| {
        paths
            .home
            .join(".agent-sync/backups")
            .join(Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string())
            .join("sync")
    });
    progress.backup_root = apply_backup_root.clone();
    let applied = apply_pack(
        paths,
        pack,
        &config.targets,
        ApplyOptions {
            dry_run: false,
            backup_root: apply_backup_root,
            allow_updates: config.allow_updates,
        },
    )?;
    let applied_before = ChangeCounts::from_changes(&applied.changes);
    progress.before = applied_before;
    progress.apply_completed = true;
    progress.backup_root = applied.backup_root.clone();
    progress.phase = "verify";
    let verification = verify_pack(paths, pack, &config.targets)?;
    if !verification.ok {
        bail!(
            "post-sync verification failed: {}",
            verification.errors.join("; ")
        );
    }
    progress.verification_ok = true;
    let final_changes = diff_pack(paths, pack, &config.targets)?;
    let after = ChangeCounts::from_changes(&final_changes);
    progress.after = after;
    let final_preserved = final_changes
        .iter()
        .filter(|change| change.action == ChangeAction::Skip)
        .map(|change| format!("{} {}", change.target, change.resource))
        .collect::<Vec<_>>();
    progress.preserved = final_preserved.clone();
    if after.has_writes() {
        bail!(
            "post-sync drift remains: {} addition(s), {} update(s)",
            after.add,
            after.update
        );
    }

    progress.phase = "history-reconcile";
    let history_hook_changed = if config.cursor_history.enabled {
        let output_dir = history_output_dir
            .as_deref()
            .context("Cursor history output directory was not resolved")?;
        let hook = install_cursor_history_hook_with_refresh(
            paths,
            executable,
            output_dir,
            false,
            config.cursor_history.refresh_qmd,
        )?;
        hook.changed
    } else {
        remove_cursor_history_hook(paths, false)?.changed
    };
    progress.history_hook_changed = history_hook_changed;
    let changed = applied_before.has_writes() || bundled_skill_changed || history_hook_changed;
    let updates_allowed = config.allow_updates || plain_update_count(&applied.changes) == 0;
    Ok(SyncReport {
        record: RunRecord {
            version: RUN_STATE_VERSION,
            run_id: String::new(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            result: if progress.cursor_history_unreadable > 0 {
                RunResult::Attention
            } else if changed {
                RunResult::Changed
            } else {
                RunResult::Healthy
            },
            applied: true,
            apply_started: true,
            executable: executable.to_path_buf(),
            agent_sync_version: env!("CARGO_PKG_VERSION").to_string(),
            executable_sha256: executable_sha256.to_string(),
            config_sha256: config_sha256.to_string(),
            manifest_sha256: Some(manifest_sha256),
            source: config.source,
            targets: config.targets.clone(),
            before: applied_before,
            after,
            preserved: final_preserved,
            verification_ok: true,
            bundled_skill_changed,
            cursor_history_checked,
            cursor_history_unreadable: progress.cursor_history_unreadable,
            history_hook_changed,
            qmd_refreshed,
            backup_root: applied.backup_root,
            failed_phase: None,
            error: None,
        },
        changes: applied.changes,
        new_preserved: Vec::new(),
        automation: options.automation,
        updates_allowed,
        skill_action: skill.action(),
        qmd_refresh_enabled: config.cursor_history.enabled && config.cursor_history.refresh_qmd,
        cursor_history_sweep_enabled: config.cursor_history.enabled,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthReport {
    pub ok: bool,
    pub checks: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl HealthReport {
    pub fn to_text(&self) -> String {
        let mut out = if self.ok {
            "Agent sync is healthy.\n".to_string()
        } else {
            "Agent sync needs attention.\n".to_string()
        };
        for check in &self.checks {
            out.push_str(&format!("ok: {check}\n"));
        }
        for warning in &self.warnings {
            out.push_str(&format!("warning: {warning}\n"));
        }
        for error in &self.errors {
            out.push_str(&format!("error: {error}\n"));
        }
        out
    }
}

pub fn doctor_managed(paths: &AgentPaths, config_path: &Path, executable: &Path) -> HealthReport {
    let mut checks = Vec::new();
    let warnings = Vec::new();
    let mut errors = Vec::new();

    let config = match load_config(config_path) {
        Ok(config) => {
            checks.push(format!("config is valid at {}", config_path.display()));
            config
        }
        Err(error) => {
            errors.push(format!("config: {error:#}"));
            return HealthReport {
                ok: false,
                checks,
                warnings,
                errors,
            };
        }
    };

    doctor_managed_with_config(
        paths,
        config_path,
        executable,
        &config,
        true,
        None,
        checks,
        warnings,
        errors,
    )
}

#[allow(clippy::too_many_arguments)]
fn doctor_managed_with_config(
    paths: &AgentPaths,
    config_path: &Path,
    executable: &Path,
    config: &Config,
    deep_cursor_history: bool,
    drift: Option<&Result<Vec<Change>>>,
    mut checks: Vec<String>,
    mut warnings: Vec<String>,
    mut errors: Vec<String>,
) -> HealthReport {
    for (label, path) in [
        ("agent sync", state_dir(paths).join("sync.lock")),
        ("QMD refresh", state_dir(paths).join("qmd-refresh.lock")),
        (
            "Cursor history export",
            state_dir(paths).join("cursor-history-export.lock"),
        ),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => errors.push(format!(
                "{label} lock path is a symlink ({})",
                path.display()
            )),
            Ok(metadata) if !metadata.is_file() => errors.push(format!(
                "{label} lock path is not a regular file ({})",
                path.display()
            )),
            Ok(_) => errors.push(if sync_lock_owner_is_active(&path) {
                format!("{label} is currently running ({})", path.display())
            } else {
                format!("{label} has an orphaned lock ({})", path.display())
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => errors.push(format!(
                "{label} lock could not be inspected at {}: {error}",
                path.display()
            )),
        }
    }

    for (label, path) in [
        ("agent-sync data", paths.home.join(".agent-sync")),
        ("run state", state_dir(paths)),
        ("temporary pack", paths.home.join(".agent-sync/tmp")),
        ("backup", paths.home.join(".agent-sync/backups")),
    ] {
        if path.exists() {
            match ensure_writable_directory_mode(&path, label) {
                Ok(()) => checks.push(format!("{label} directory is writable")),
                Err(error) => errors.push(format!("{label} directory: {error:#}")),
            }
        }
    }
    let run_history_path = state_dir(paths).join("runs.jsonl");
    if run_history_path.exists() {
        match ensure_writable_file_mode(&run_history_path, "run history") {
            Ok(()) => checks.push("run history accepts future records".to_string()),
            Err(error) => errors.push(format!("run history persistence: {error:#}")),
        }
    }

    for (label, path) in [
        ("last attempt", state_dir(paths).join("last-attempt.json")),
        ("last success", state_dir(paths).join("last-success.json")),
        ("run history", state_dir(paths).join("runs.jsonl")),
    ] {
        match ensure_regular_file(&path, label) {
            Ok(()) => checks.push(format!("{label} state file is present and safe")),
            Err(error) => errors.push(format!("{label} state: {error:#}")),
        }
    }

    if matches!(drift, Some(Ok(_))) {
        checks.push("source, target, and MCP policy passed preflight".to_string());
    } else if let Err(error) = preflight(paths, config) {
        errors.push(format!("preflight: {error:#}"));
    } else {
        checks.push("source, target, and MCP policy passed preflight".to_string());
    }

    match install_agent_skills(paths, AgentSkillInstallOptions { dry_run: true }) {
        Ok(report) if report.action() == AgentSkillInstallAction::Unchanged => {
            checks.push("natural-language agent-sync skill is installed".to_string());
        }
        Ok(report) => errors.push(format!(
            "natural-language skill needs {:?}: {}",
            report.action(),
            report
                .installations
                .iter()
                .filter(|installation| {
                    installation.action != AgentSkillInstallAction::Unchanged
                })
                .map(|installation| installation.destination.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Err(error) => errors.push(format!("natural-language skill: {error:#}")),
    }

    let history_output_dir = if config.cursor_history.enabled {
        match cursor_history_output_dir(paths, config) {
            Ok(output_dir) => Some(output_dir),
            Err(error) => {
                errors.push(format!("Cursor history output: {error:#}"));
                None
            }
        }
    } else {
        None
    };
    if let Some(output_dir) = history_output_dir.as_deref() {
        match install_cursor_history_hook_with_refresh(
            paths,
            executable,
            output_dir,
            true,
            config.cursor_history.refresh_qmd,
        ) {
            Ok(report) if !report.changed => {
                checks.push("Cursor history hook matches this executable".to_string())
            }
            Ok(_) => errors.push("Cursor history hook is missing or stale".to_string()),
            Err(error) => errors.push(format!("Cursor history hook: {error:#}")),
        }
        let history_coverage = if deep_cursor_history {
            match cursor_history_coverage_at(paths, output_dir) {
                Ok(coverage) if coverage.is_complete() => {
                    checks.push(format!(
                        "{} Cursor transcript(s) have current Markdown exports",
                        coverage.transcripts
                    ));
                    Some(coverage)
                }
                Ok(coverage) => {
                    errors.push(format!(
                        "Cursor history coverage is incomplete: {} missing, {} stale, {} unreadable",
                        coverage.missing.len(),
                        coverage.stale.len(),
                        coverage.unreadable.len()
                    ));
                    None
                }
                Err(error) => {
                    errors.push(format!("Cursor history coverage: {error:#}"));
                    None
                }
            }
        } else {
            report_unreadable_cursor_history(paths, &mut errors);
            None
        };
        if config.cursor_history.refresh_qmd {
            match qmd_executable(paths) {
                Some(path) => checks.push(format!("QMD is available at {}", path.display())),
                None => errors.push("QMD executable was not found in a standard path".to_string()),
            }
            match qmd_refresh_last_success(paths) {
                Ok(Some(last)) => {
                    checks.push(format!("QMD last refreshed at {}", last.to_rfc3339()))
                }
                Ok(None) => errors.push("QMD has no recorded successful refresh".to_string()),
                Err(error) => errors.push(format!("QMD refresh state: {error:#}")),
            }
            match qmd_health(paths) {
                Ok(health) => {
                    match verify_qmd_sessions_path(output_dir, &health.sessions_path) {
                        Ok(expected) => checks.push(format!(
                            "QMD sessions collection covers {}",
                            expected.display()
                        )),
                        Err(error) => errors.push(format!("QMD health: {error:#}")),
                    }
                    if health.pending_embeddings == 0 {
                        checks.push("QMD has no pending embeddings".to_string());
                    } else {
                        warnings.push(format!(
                            "QMD reports {} pending embedding(s); agent-sync exports are checked separately",
                            health.pending_embeddings
                        ));
                    }
                }
                Err(error) => errors.push(format!("QMD health: {error:#}")),
            }
            if let Some(coverage) = &history_coverage {
                match qmd_missing_exports(paths, &coverage.expected_exports) {
                    Ok(missing_from_qmd) if missing_from_qmd.is_empty() => checks.push(format!(
                        "{} Cursor history export(s) are retrievable from QMD",
                        coverage.expected_exports.len()
                    )),
                    Ok(missing_from_qmd) => errors.push(format!(
                        "{} Cursor history export(s) are missing from QMD: {}",
                        missing_from_qmd.len(),
                        missing_from_qmd
                            .iter()
                            .map(|export| export.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    Err(error) => {
                        errors.push(format!("QMD export lookup: {error:#}"));
                    }
                }
            }
            match qmd_pending_exports(paths) {
                Ok(0) => checks.push("no Cursor history exports await QMD indexing".to_string()),
                Ok(count) => errors.push(format!(
                    "{count} Cursor history export(s) still await QMD indexing"
                )),
                Err(error) => errors.push(format!("QMD pending export state: {error:#}")),
            }
        }
    } else if !config.cursor_history.enabled {
        match remove_cursor_history_hook(paths, true) {
            Ok(report) if !report.changed => {
                checks.push("managed Cursor history hook is disabled".to_string())
            }
            Ok(_) => errors.push(
                "managed Cursor history hook is still installed while history is disabled"
                    .to_string(),
            ),
            Err(error) => errors.push(format!("Cursor history hook disable check: {error:#}")),
        }
    }

    let owned_drift;
    let drift = if let Some(drift) = drift {
        drift
    } else {
        owned_drift = current_changes(paths, config);
        &owned_drift
    };
    match drift {
        Ok(changes) => {
            let counts = ChangeCounts::from_changes(changes);
            if counts.has_writes() {
                errors.push(format!(
                    "configuration drift: {} addition(s), {} update(s); run agent-sync sync",
                    counts.add, counts.update
                ));
            } else {
                checks.push("no writable configuration drift".to_string());
            }
            if counts.preserved > 0 {
                warnings.push(format!(
                    "{} target-owned resource(s) are intentionally preserved",
                    counts.preserved
                ));
            }
        }
        Err(error) => errors.push(format!("drift check: {error:#}")),
    }

    let last_success = load_last_success(paths);
    match &last_success {
        Ok(Some(record)) => {
            let age = Utc::now().signed_duration_since(record.finished_at);
            if age.num_hours() > config.health.stale_after_hours as i64 {
                errors.push(format!(
                    "last successful applied sync is stale: {}",
                    record.finished_at.to_rfc3339()
                ));
            } else {
                checks.push(format!(
                    "last successful applied sync was {}",
                    record.finished_at.to_rfc3339()
                ));
            }
            match hash_path(executable) {
                Ok(hash) if hash == record.executable_sha256 => checks.push(format!(
                    "last success used this agent-sync {} binary",
                    record.agent_sync_version
                )),
                Ok(_) => errors.push(
                    "the installed binary changed after the last successful sync; run agent-sync sync"
                        .to_string(),
                ),
                Err(error) => errors.push(format!("installed binary hash: {error:#}")),
            }
            match hash_path(config_path) {
                Ok(hash) if hash == record.config_sha256 => {
                    checks.push("last success used the current config".to_string())
                }
                Ok(_) => errors.push(
                    "the managed config changed after the last successful sync; run agent-sync sync"
                        .to_string(),
                ),
                Err(error) => errors.push(format!("managed config hash: {error:#}")),
            }
        }
        Ok(None) => errors.push("no successful applied sync is recorded".to_string()),
        Err(error) => errors.push(format!("last successful run: {error:#}")),
    }

    let last_attempt = load_last_attempt(paths);
    match &last_attempt {
        Ok(Some(attempt)) if attempt.result == RunResult::Failed => {
            errors.push(format!(
                "the latest sync attempt failed at {}: {}",
                attempt.finished_at.to_rfc3339(),
                attempt.error.as_deref().unwrap_or("unknown error")
            ));
        }
        Ok(_) => {}
        Err(error) => errors.push(format!("last sync attempt: {error:#}")),
    }

    match load_latest_history_record(paths) {
        Ok(Some(history)) => {
            match &last_attempt {
                Ok(Some(attempt)) if attempt.run_id == history.run_id => {
                    checks.push("run history matches the last-attempt snapshot".to_string())
                }
                Ok(Some(attempt)) => errors.push(format!(
                    "run history latest id {} does not match last-attempt id {}",
                    history.run_id, attempt.run_id
                )),
                Ok(None) => errors.push(
                    "run history exists but the last-attempt snapshot is missing".to_string(),
                ),
                Err(_) => {}
            }
            if history.applied && history.result != RunResult::Failed {
                match &last_success {
                    Ok(Some(success)) if success.run_id == history.run_id => checks
                        .push("last-success snapshot matches the latest applied run".to_string()),
                    Ok(Some(success)) => errors.push(format!(
                        "latest applied run {} is missing from last-success snapshot {}",
                        history.run_id, success.run_id
                    )),
                    Ok(None) => errors.push(
                        "run history contains an applied run but last-success is missing"
                            .to_string(),
                    ),
                    Err(_) => {}
                }
            }
        }
        Ok(None) => {}
        Err(error) => errors.push(format!("run history: {error:#}")),
    }

    HealthReport {
        ok: errors.is_empty(),
        checks,
        warnings,
        errors,
    }
}

fn report_unreadable_cursor_history(paths: &AgentPaths, errors: &mut Vec<String>) {
    match cursor_history_unreadable_count(paths) {
        Ok(0) => {}
        Ok(count) => errors.push(format!(
            "Cursor history coverage is incomplete: {count} unreadable transcript(s)"
        )),
        Err(error) => errors.push(format!("Cursor history coverage: {error:#}")),
    }
}

pub fn status_managed(paths: &AgentPaths, config_path: &Path, executable: &Path) -> Result<String> {
    Ok(status_managed_report(paths, config_path, executable)?.to_text())
}

pub fn status_managed_report(
    paths: &AgentPaths,
    config_path: &Path,
    executable: &Path,
) -> Result<ManagedStatus> {
    let config = load_config(config_path)?;
    let drift = current_changes(paths, &config);
    let counts = match &drift {
        Ok(changes) => ChangeCounts::from_changes(changes),
        Err(_) => ChangeCounts::default(),
    };
    let health = doctor_managed_with_config(
        paths,
        config_path,
        executable,
        &config,
        false,
        Some(&drift),
        vec![format!("config is valid at {}", config_path.display())],
        Vec::new(),
        Vec::new(),
    );
    let last_success = load_last_success(paths)?.map(|record| StatusLastSuccess {
        finished_at: record.finished_at,
        result: record.result,
        agent_sync_version: record.agent_sync_version,
    });
    let next_action = if last_success.is_none() || counts.has_writes() {
        StatusNextAction::PreviewSync
    } else if !health.ok {
        StatusNextAction::RunDoctor
    } else {
        StatusNextAction::None
    };
    let cursor_history = CursorHistoryMode::from_config(&config);

    Ok(ManagedStatus {
        source: config.source,
        targets: config.targets,
        cursor_history,
        drift: counts,
        last_success,
        healthy: health.ok,
        warnings: health.warnings,
        errors: health.errors,
        next_action,
    })
}

fn format_config_summary(config: &Config) -> String {
    let mcp = match config.mcp.mode {
        McpMode::None => "none".to_string(),
        McpMode::All => "all source servers".to_string(),
        McpMode::Selected => format!("selected ({})", config.mcp.servers.join(", ")),
    };
    let history = if config.cursor_history.enabled {
        if config.cursor_history.refresh_qmd {
            "enabled with QMD refresh"
        } else {
            "enabled without QMD refresh"
        }
    } else {
        "disabled"
    };
    format!(
        "Route: {} -> {}\nMCP: {mcp}\nReferences: {}\nTarget replacements: {}\nCursor history: {history}\nStale after: {} hours\n",
        config.source.agent_kind(),
        config
            .targets
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        if config.include_references { "included" } else { "excluded" },
        if config.allow_updates { "allowed" } else { "blocked" },
        config.health.stale_after_hours
    )
}

fn cursor_history_output_dir(paths: &AgentPaths, _config: &Config) -> Result<PathBuf> {
    Ok(default_cursor_history_output_dir(paths))
}

fn verify_qmd_health(paths: &AgentPaths, output_dir: &Path) -> Result<()> {
    let health = qmd_health(paths)?;
    verify_qmd_sessions_path(output_dir, &health.sessions_path)?;
    Ok(())
}

fn verify_qmd_sessions_path(expected: &Path, sessions_path: &Path) -> Result<PathBuf> {
    let actual_path =
        fs::canonicalize(sessions_path).unwrap_or_else(|_| sessions_path.to_path_buf());
    let expected_path = fs::canonicalize(expected).unwrap_or_else(|_| expected.to_path_buf());
    if actual_path != expected_path {
        bail!(
            "QMD sessions collection points to {}, expected {}",
            actual_path.display(),
            expected_path.display()
        );
    }
    Ok(expected_path)
}

fn current_changes(paths: &AgentPaths, config: &Config) -> Result<Vec<Change>> {
    preflight(paths, config)?;
    let temp = tempfile::Builder::new()
        .prefix("agent-sync-status-")
        .tempdir()
        .context("create private status pack")?;
    export_pack(paths, temp.path(), export_options(config))?;
    diff_pack(paths, temp.path(), &config.targets)
}

fn export_options(config: &Config) -> ExportOptions {
    ExportOptions {
        source: match config.source {
            CanonicalSource::Codex => SourceSelection::Codex,
            CanonicalSource::Claude => SourceSelection::Claude,
            CanonicalSource::Cursor => SourceSelection::Cursor,
        },
        include_references: config.include_references,
        include_mcp: config.mcp.mode != McpMode::None,
        mcp_servers: match config.mcp.mode {
            McpMode::Selected => config.mcp.servers.clone(),
            McpMode::None | McpMode::All => Vec::new(),
        },
    }
}

fn preflight(paths: &AgentPaths, config: &Config) -> Result<()> {
    config.validate()?;
    let source_root = match config.source {
        CanonicalSource::Codex => &paths.codex_home,
        CanonicalSource::Claude => &paths.claude_home,
        CanonicalSource::Cursor => &paths.cursor_home,
    };
    if !source_root.is_dir() {
        bail!("source directory does not exist: {}", source_root.display());
    }
    if config.targets.contains(&AgentKind::Cursor) && config.mcp.mode != McpMode::None {
        ensure_cursor_mcp_write_safe(&paths.cursor_config)?;
    }
    if config.mcp.mode == McpMode::Selected {
        let available = match config.source {
            CanonicalSource::Codex => discover_codex_mcp(&paths.codex_home.join("config.toml"))?,
            CanonicalSource::Claude => discover_claude_mcp(&paths.claude_config)?,
            CanonicalSource::Cursor => discover_cursor_mcp(&paths.cursor_config)?,
        };
        let missing = config
            .mcp
            .servers
            .iter()
            .filter(|name| !available.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "selected MCP servers are missing from the source: {}",
                missing.join(", ")
            );
        }
    }
    Ok(())
}

fn state_dir(paths: &AgentPaths) -> PathBuf {
    paths.home.join(".agent-sync/state")
}

fn load_last_success(paths: &AgentPaths) -> Result<Option<RunRecord>> {
    load_run_record(&state_dir(paths).join("last-success.json"))
}

fn load_last_attempt(paths: &AgentPaths) -> Result<Option<RunRecord>> {
    load_run_record(&state_dir(paths).join("last-attempt.json"))
}

fn load_latest_history_record(paths: &AgentPaths) -> Result<Option<RunRecord>> {
    let path = state_dir(paths).join("runs.jsonl");
    let Some(raw) = read_to_string_if_exists(&path)? else {
        return Ok(None);
    };
    let Some(line) = raw.lines().rev().find(|line| !line.trim().is_empty()) else {
        return Ok(None);
    };
    let record = serde_json::from_str(line)
        .with_context(|| format!("parse latest run history record {}", path.display()))?;
    Ok(Some(record))
}

fn load_run_record(path: &Path) -> Result<Option<RunRecord>> {
    let Some(raw) = read_to_string_if_exists(path)? else {
        return Ok(None);
    };
    let record = serde_json::from_str(&raw)
        .with_context(|| format!("parse run record {}", path.display()))?;
    Ok(Some(record))
}

fn write_run_record(paths: &AgentPaths, record: &RunRecord, success: bool) -> Result<()> {
    let state = state_dir(paths);
    ensure_dir(&state)?;
    let line = serde_json::to_vec(record)?;
    let pretty = [serde_json::to_vec_pretty(record)?, b"\n".to_vec()].concat();
    let last_attempt = state.join("last-attempt.json");
    let last_success = state.join("last-success.json");
    let runs = state.join("runs.jsonl");
    ensure_regular_or_missing(&last_attempt, "last attempt")?;
    ensure_regular_or_missing(&last_success, "last success")?;
    ensure_regular_or_missing(&runs, "run history")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&runs)
        .with_context(|| format!("open run history {}", runs.display()))?;
    file.write_all(&line)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    write_atomic(&last_attempt, &pretty)?;
    if success {
        write_atomic(&last_success, &pretty)?;
    }
    Ok(())
}

fn preflight_run_state(paths: &AgentPaths) -> Result<()> {
    let state = state_dir(paths);
    ensure_dir(&state)?;
    let last_attempt = state.join("last-attempt.json");
    let last_success = state.join("last-success.json");
    let runs = state.join("runs.jsonl");
    ensure_regular_or_missing(&last_attempt, "last attempt")?;
    ensure_regular_or_missing(&last_success, "last success")?;
    ensure_regular_or_missing(&runs, "run history")?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&runs)
        .with_context(|| format!("open run history {}", runs.display()))?;
    Ok(())
}

fn ensure_regular_or_missing(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to replace symlinked {label} {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("{label} path is not a regular file: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to read symlinked {label} {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("{label} path is not a regular file: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!("{label} file is missing: {}", path.display())
        }
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn ensure_writable_directory_mode(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "{label} path is not a regular directory: {}",
            path.display()
        );
    }
    ensure_permissions_have_write_bit(&metadata, label, path)
}

fn ensure_writable_file_mode(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label} path is not a regular file: {}", path.display());
    }
    ensure_permissions_have_write_bit(&metadata, label, path)
}

fn ensure_permissions_have_write_bit(
    metadata: &fs::Metadata,
    label: &str,
    path: &Path,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o222 == 0 {
            bail!("{label} is not writable: {}", path.display());
        }
    }
    #[cfg(not(unix))]
    if metadata.permissions().readonly() {
        bail!("{label} is not writable: {}", path.display());
    }
    Ok(())
}

struct SyncLock {
    path: PathBuf,
    token: String,
}

struct SyncLockSnapshot {
    token: String,
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SyncLock {
    fn acquire(paths: &AgentPaths) -> Result<Self> {
        let state = state_dir(paths);
        ensure_dir(&state)?;
        let path = state.join("sync.lock");
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let token = format!("{}:{}", std::process::id(), Utc::now().timestamp_millis());
                    writeln!(file, "{token}")?;
                    file.sync_all()?;
                    return Ok(Self { path, token });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let Some(snapshot) = read_sync_lock_snapshot(&path)? else {
                        continue;
                    };
                    if lock_token_owner_is_active(&snapshot.token)
                        && !sync_lock_hard_expired(&snapshot)
                    {
                        bail!("another agent-sync run is active ({})", path.display());
                    }
                    if remove_sync_lock_if_unchanged(&path, &snapshot)? {
                        continue;
                    }
                    bail!("agent-sync lock changed while checking it; retry the command");
                }
                Err(error) => return Err(error).context("create sync lock"),
            }
        }
        bail!("could not acquire sync lock {}", path.display())
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.path).is_ok_and(|current| current.trim() == self.token) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn sync_lock_owner_is_active(path: &Path) -> bool {
    read_sync_lock_snapshot(path)
        .ok()
        .flatten()
        .is_some_and(|snapshot| {
            !sync_lock_hard_expired(&snapshot) && lock_token_owner_is_active(&snapshot.token)
        })
}

fn sync_lock_hard_expired(snapshot: &SyncLockSnapshot) -> bool {
    let now = Utc::now().timestamp_millis();
    let token_age = snapshot
        .token
        .split_once(':')
        .and_then(|(_, timestamp)| timestamp.trim().parse::<i64>().ok())
        .and_then(|timestamp| now.checked_sub(timestamp))
        .and_then(|millis| u64::try_from(millis).ok())
        .map(Duration::from_millis);
    let modified_age = snapshot.modified.elapsed().ok();
    (match (token_age, modified_age) {
        (Some(token), Some(modified)) => token.max(modified),
        (Some(age), None) | (None, Some(age)) => age,
        (None, None) => return false,
    }) >= PERSISTENT_LOCK_HARD_MAX_AGE
}

fn lock_token_owner_is_active(token: &str) -> bool {
    let Some(pid) = token
        .split(':')
        .next()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
    else {
        return false;
    };
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn read_sync_lock_snapshot(path: &Path) -> Result<Option<SyncLockSnapshot>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("sync lock path is a symlink: {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("sync lock path is not a regular file: {}", path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect lock {}", path.display()))
        }
    };
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open lock {}", path.display())),
    };
    let file_metadata = file
        .metadata()
        .with_context(|| format!("inspect open lock {}", path.display()))?;
    if !file_metadata.is_file() {
        bail!("sync lock path is not a regular file: {}", path.display());
    }
    let mut token = String::new();
    file.read_to_string(&mut token)
        .with_context(|| format!("read lock {}", path.display()))?;
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("sync lock path is a symlink: {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("sync lock path is not a regular file: {}", path.display())
        }
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect lock {}", path.display()))
        }
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let same_file = file_metadata.len() == path_metadata.len()
        && file_metadata.modified()? == path_metadata.modified()?;
    #[cfg(unix)]
    let same_file = same_file
        && file_metadata.dev() == path_metadata.dev()
        && file_metadata.ino() == path_metadata.ino();
    if !same_file {
        return Ok(None);
    }
    Ok(Some(SyncLockSnapshot {
        token,
        modified: file_metadata.modified()?,
        len: file_metadata.len(),
        #[cfg(unix)]
        device: file_metadata.dev(),
        #[cfg(unix)]
        inode: file_metadata.ino(),
    }))
}

fn remove_sync_lock_if_unchanged(path: &Path, expected: &SyncLockSnapshot) -> Result<bool> {
    let Some(current) = read_sync_lock_snapshot(path)? else {
        return Ok(true);
    };
    let matches = current.token == expected.token
        && current.modified == expected.modified
        && current.len == expected.len;
    #[cfg(unix)]
    let matches = matches && current.device == expected.device && current.inode == expected.inode;
    if !matches {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).context("remove stale sync lock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_replaces_a_lock_owned_by_a_dead_process_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let state = state_dir(&paths);
        ensure_dir(&state).unwrap();
        let path = state.join("sync.lock");
        fs::write(
            &path,
            format!("{}:{}\n", u32::MAX, Utc::now().timestamp_millis()),
        )
        .unwrap();

        let lock = SyncLock::acquire(&paths).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap().trim(), lock.token);
        drop(lock);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn acquire_replaces_a_hard_expired_lock_with_a_reused_live_pid() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let state = state_dir(&paths);
        ensure_dir(&state).unwrap();
        let path = state.join("sync.lock");
        fs::write(&path, format!("{}:0\n", std::process::id())).unwrap();

        let expired = read_sync_lock_snapshot(&path).unwrap().unwrap();
        assert!(lock_token_owner_is_active(&expired.token));
        assert!(sync_lock_hard_expired(&expired));

        let lock = SyncLock::acquire(&paths).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), lock.token);
        drop(lock);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn routine_status_does_not_run_exact_qmd_export_lookups() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        fs::create_dir_all(&paths.codex_home).unwrap();
        fs::create_dir_all(&paths.cursor_home).unwrap();
        let transcript = paths
            .cursor_home
            .join("projects/example/agent-transcripts/session/session.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
        )
        .unwrap();
        crate::cursor_history::export_cursor_history(
            &paths,
            &serde_json::json!({
                "conversation_id": "session",
                "transcript_path": transcript,
            }),
            None,
            false,
        )
        .unwrap();

        let mut config = Config::default();
        config.cursor_history.enabled = true;
        config.cursor_history.refresh_qmd = true;
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, render_config(&config).unwrap()).unwrap();

        let command_log = temp.path().join("qmd-commands.log");
        let qmd = paths.home.join(".local/bin/qmd");
        fs::create_dir_all(qmd.parent().unwrap()).unwrap();
        let output_dir = default_cursor_history_output_dir(&paths);
        fs::write(
            &qmd,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "printf '%s\\n' \"$1\" >> {}\n",
                    "case \"$1\" in\n",
                    "collection) printf '%s\\n' 'Path: {}' 'Pattern: **/*.md' 'Include: yes (default)' ;;\n",
                    "status) printf '%s\\n' 'Pending: 0 need embedding' ;;\n",
                    "multi-get) printf '[]\\n' ;;\n",
                    "*) exit 9 ;;\n",
                    "esac\n"
                ),
                test_shell_quote(&command_log),
                output_dir.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&qmd, fs::Permissions::from_mode(0o755)).unwrap();

        status_managed_report(&paths, &config_path, &std::env::current_exe().unwrap()).unwrap();

        assert_eq!(
            fs::read_to_string(command_log).unwrap(),
            "collection\nstatus\n"
        );
    }

    #[cfg(unix)]
    fn test_shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
}

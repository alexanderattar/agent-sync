use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    adapters::AgentPaths,
    fsx::{ensure_dir, replace_file_with_backup, write_atomic},
};

pub const BUNDLED_AGENT_SKILL_NAME: &str = "agent-sync";
pub const BUNDLED_AGENT_SKILL: &str = include_str!("../skills/agent-sync/SKILL.md");

const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentSkillInstallOptions {
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSkillInstallAction {
    Add,
    Update,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSkillInstallReport {
    pub action: AgentSkillInstallAction,
    pub dry_run: bool,
    pub destination: PathBuf,
    pub state_path: PathBuf,
    pub backup: Option<PathBuf>,
}

impl AgentSkillInstallReport {
    pub fn to_text(&self) -> String {
        let action = match (self.dry_run, self.action) {
            (true, AgentSkillInstallAction::Add) => "Dry run. Add",
            (true, AgentSkillInstallAction::Update) => "Dry run. Update",
            (_, AgentSkillInstallAction::Unchanged) => "Unchanged",
            (false, AgentSkillInstallAction::Add) => "Installed",
            (false, AgentSkillInstallAction::Update) => "Updated",
        };
        let mut out = format!(
            "{action} bundled agent-sync skill -> {}\n",
            self.destination.display()
        );
        if let Some(backup) = &self.backup {
            out.push_str(&format!("Backup: {}\n", backup.display()));
        }
        out
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct BundledSkillState {
    schema_version: u32,
    destination: PathBuf,
    installed_sha256: String,
}

/// Installs the bundled agent-sync skill into the shared user skill directory.
///
/// The installer only updates a copy recorded in agent-sync state whose current
/// hash still matches that record. Any other differing existing copy is treated
/// as user-owned and is left unchanged.
pub fn install_agent_skill(
    paths: &AgentPaths,
    options: AgentSkillInstallOptions,
) -> Result<AgentSkillInstallReport> {
    let destination = skill_destination(paths);
    let state_path = skill_state_path(paths);
    ensure_safe_state_path(&state_path)?;
    let action = planned_action(&destination, &state_path)?;

    if options.dry_run {
        return Ok(AgentSkillInstallReport {
            action,
            dry_run: options.dry_run,
            destination,
            state_path,
            backup: None,
        });
    }

    if action == AgentSkillInstallAction::Unchanged {
        return Ok(AgentSkillInstallReport {
            action,
            dry_run: false,
            destination,
            state_path,
            backup: None,
        });
    }

    let original_state = read_optional_bytes(&state_path)?;
    let original_skill = if action == AgentSkillInstallAction::Update {
        Some(
            fs::read(&destination)
                .with_context(|| format!("read agent-sync skill {}", destination.display()))?,
        )
    } else {
        None
    };
    let backup = match action {
        AgentSkillInstallAction::Add => {
            create_new_skill(&destination)?;
            None
        }
        AgentSkillInstallAction::Update => {
            // Recheck ownership immediately before replacing the managed file.
            require_unmodified_managed_copy(&destination, &state_path)?;
            let backup_root = paths
                .home
                .join(".agent-sync")
                .join("backups")
                .join(Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string())
                .join("bundled-skill");
            replace_file_with_backup(
                &backup_root,
                &paths.home,
                &destination,
                BUNDLED_AGENT_SKILL.as_bytes(),
            )?
        }
        AgentSkillInstallAction::Unchanged => unreachable!("handled above"),
    };

    if let Err(error) = write_state(&state_path, &destination) {
        let mut rollback_errors = Vec::new();
        match action {
            AgentSkillInstallAction::Add => {
                if let Err(rollback_error) = fs::remove_file(&destination) {
                    if rollback_error.kind() != ErrorKind::NotFound {
                        rollback_errors.push(format!(
                            "remove new skill {}: {rollback_error}",
                            destination.display()
                        ));
                    }
                }
                if let Some(skill_dir) = destination.parent() {
                    if let Err(rollback_error) = fs::remove_dir(skill_dir) {
                        if rollback_error.kind() != ErrorKind::NotFound {
                            rollback_errors.push(format!(
                                "remove new skill directory {}: {rollback_error}",
                                skill_dir.display()
                            ));
                        }
                    }
                }
            }
            AgentSkillInstallAction::Update => {
                if let Some(content) = original_skill.as_deref() {
                    if let Err(rollback_error) = write_atomic(&destination, content) {
                        rollback_errors.push(format!(
                            "restore skill {}: {rollback_error:#}",
                            destination.display()
                        ));
                    }
                }
            }
            AgentSkillInstallAction::Unchanged => unreachable!("handled above"),
        }
        if let Err(rollback_error) = restore_optional_file(&state_path, original_state.as_deref()) {
            rollback_errors.push(format!(
                "restore skill state {}: {rollback_error:#}",
                state_path.display()
            ));
        }
        if rollback_errors.is_empty() {
            return Err(error)
                .context("write bundled skill ownership state; skill was rolled back");
        }
        bail!(
            "write bundled skill ownership state failed: {error:#}; rollback also failed: {}",
            rollback_errors.join("; ")
        );
    }
    Ok(AgentSkillInstallReport {
        action,
        dry_run: false,
        destination,
        state_path,
        backup,
    })
}

pub fn skill_destination(paths: &AgentPaths) -> PathBuf {
    paths
        .agents_home
        .join("skills")
        .join(BUNDLED_AGENT_SKILL_NAME)
        .join("SKILL.md")
}

pub fn skill_state_path(paths: &AgentPaths) -> PathBuf {
    paths
        .home
        .join(".agent-sync")
        .join("state")
        .join("bundled-skill.json")
}

fn planned_action(destination: &Path, state_path: &Path) -> Result<AgentSkillInstallAction> {
    let skill_dir = destination
        .parent()
        .context("bundled agent skill destination has no parent")?;
    let skill_dir_metadata = symlink_metadata_if_exists(skill_dir)?;
    let Some(skill_dir_metadata) = skill_dir_metadata else {
        if let Some(state) = load_state(state_path)? {
            validate_state_identity(&state, destination, state_path)?;
        }
        return Ok(AgentSkillInstallAction::Add);
    };
    if skill_dir_metadata.file_type().is_symlink() {
        bail!(
            "refusing to replace symlinked agent-sync skill directory {}",
            skill_dir.display()
        );
    }
    if !skill_dir_metadata.is_dir() {
        bail!(
            "refusing to replace unmanaged agent-sync skill path {}",
            skill_dir.display()
        );
    }

    let Some(destination_metadata) = symlink_metadata_if_exists(destination)? else {
        bail!(
            "refusing to modify unmanaged agent-sync skill directory {}; SKILL.md is missing",
            skill_dir.display()
        );
    };
    if destination_metadata.file_type().is_symlink() {
        bail!(
            "refusing to replace symlinked agent-sync skill file {}",
            destination.display()
        );
    }
    if !destination_metadata.is_file() {
        bail!(
            "refusing to replace unmanaged agent-sync skill path {}",
            destination.display()
        );
    }

    let current = fs::read(destination)
        .with_context(|| format!("read agent-sync skill {}", destination.display()))?;
    if current == BUNDLED_AGENT_SKILL.as_bytes() {
        require_unmodified_managed_copy(destination, state_path)?;
        return Ok(AgentSkillInstallAction::Unchanged);
    }

    require_unmodified_managed_copy(destination, state_path)?;
    Ok(AgentSkillInstallAction::Update)
}

fn require_unmodified_managed_copy(destination: &Path, state_path: &Path) -> Result<()> {
    let state = load_state(state_path)?.with_context(|| {
        format!(
            "refusing to replace unmanaged agent-sync skill {}",
            destination.display()
        )
    })?;
    validate_state_identity(&state, destination, state_path)?;

    let current = fs::read(destination)
        .with_context(|| format!("read agent-sync skill {}", destination.display()))?;
    let current_sha256 = sha256(&current);
    if current_sha256 != state.installed_sha256 {
        bail!(
            "refusing to replace user-modified agent-sync skill {}; current hash does not match {}",
            destination.display(),
            state_path.display()
        );
    }
    Ok(())
}

fn validate_state_identity(
    state: &BundledSkillState,
    destination: &Path,
    state_path: &Path,
) -> Result<()> {
    if state.schema_version != STATE_SCHEMA_VERSION {
        bail!(
            "unsupported bundled skill state version {} in {}",
            state.schema_version,
            state_path.display()
        );
    }
    if state.destination != destination {
        bail!(
            "refusing to use bundled skill state for {}; it records {}",
            destination.display(),
            state.destination.display()
        );
    }
    Ok(())
}

fn create_new_skill(destination: &Path) -> Result<()> {
    let skill_dir = destination
        .parent()
        .context("bundled agent skill destination has no parent")?;
    let skills_dir = skill_dir
        .parent()
        .context("bundled agent skill directory has no parent")?;
    ensure_dir(skills_dir)?;
    fs::create_dir(skill_dir)
        .with_context(|| format!("create agent-sync skill directory {}", skill_dir.display()))?;
    if let Err(error) = write_atomic(destination, BUNDLED_AGENT_SKILL.as_bytes()) {
        let _ = fs::remove_dir(skill_dir);
        return Err(error);
    }
    Ok(())
}

fn load_state(path: &Path) -> Result<Option<BundledSkillState>> {
    if symlink_metadata_if_exists(path)?.is_none() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read bundled skill state {}", path.display()))?;
    let state = serde_json::from_str(&raw)
        .with_context(|| format!("parse bundled skill state {}", path.display()))?;
    Ok(Some(state))
}

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn restore_optional_file(path: &Path, content: Option<&[u8]>) -> Result<()> {
    match content {
        Some(content) => write_atomic(path, content),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        },
    }
}

fn ensure_safe_state_path(path: &Path) -> Result<()> {
    let Some(metadata) = symlink_metadata_if_exists(path)? else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to read or replace symlinked bundled skill state {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "bundled skill state path {} is not a regular file",
            path.display()
        );
    }
    Ok(())
}

fn write_state(path: &Path, destination: &Path) -> Result<()> {
    let state = BundledSkillState {
        schema_version: STATE_SCHEMA_VERSION,
        destination: destination.to_path_buf(),
        installed_sha256: sha256(BUNDLED_AGENT_SKILL.as_bytes()),
    };
    let content = [serde_json::to_vec_pretty(&state)?, b"\n".to_vec()].concat();
    write_atomic(path, &content)
}

fn symlink_metadata_if_exists(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn sha256(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

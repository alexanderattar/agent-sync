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
    fsx::{
        ensure_dir, remove_target_if_unchanged, replace_file_if_unchanged,
        replace_file_with_backup_if_unchanged, restore_backup_atomically_if_unchanged,
        write_atomic,
    },
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSkillInstallSetReport {
    pub installations: Vec<AgentSkillInstallReport>,
}

impl AgentSkillInstallSetReport {
    pub fn action(&self) -> AgentSkillInstallAction {
        if self
            .installations
            .iter()
            .any(|report| report.action == AgentSkillInstallAction::Update)
        {
            AgentSkillInstallAction::Update
        } else if self
            .installations
            .iter()
            .any(|report| report.action == AgentSkillInstallAction::Add)
        {
            AgentSkillInstallAction::Add
        } else {
            AgentSkillInstallAction::Unchanged
        }
    }

    pub fn to_text(&self) -> String {
        self.installations
            .iter()
            .map(AgentSkillInstallReport::to_text)
            .collect()
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
    install_agent_skill_at(
        paths,
        skill_destination(paths),
        skill_state_path(paths),
        options,
    )
}

/// Installs the control skill where Codex, Cursor, and Claude Code discover it.
pub fn install_agent_skills(
    paths: &AgentPaths,
    options: AgentSkillInstallOptions,
) -> Result<AgentSkillInstallSetReport> {
    install_agent_skills_with(paths, options, install_agent_skill_at)
}

fn install_agent_skills_with<F>(
    paths: &AgentPaths,
    options: AgentSkillInstallOptions,
    mut installer: F,
) -> Result<AgentSkillInstallSetReport>
where
    F: FnMut(
        &AgentPaths,
        PathBuf,
        PathBuf,
        AgentSkillInstallOptions,
    ) -> Result<AgentSkillInstallReport>,
{
    let destinations = [
        (skill_destination(paths), skill_state_path(paths)),
        (
            claude_skill_destination(paths),
            claude_skill_state_path(paths),
        ),
    ];
    let previews = destinations
        .iter()
        .map(|(destination, state_path)| {
            install_agent_skill_at(
                paths,
                destination.clone(),
                state_path.clone(),
                AgentSkillInstallOptions { dry_run: true },
            )
        })
        .collect::<Result<Vec<_>>>()?;
    if options.dry_run {
        return Ok(AgentSkillInstallSetReport {
            installations: previews,
        });
    }

    let snapshots = destinations
        .iter()
        .map(|(destination, state_path)| InstallSnapshot::capture(destination, state_path))
        .collect::<Result<Vec<_>>>()?;
    let mut installations = Vec::with_capacity(destinations.len());
    for (destination, state_path) in destinations {
        match installer(paths, destination, state_path, options) {
            Ok(report) => installations.push(report),
            Err(error) => {
                let rollback_errors = rollback_install_set(&installations, &snapshots);
                if rollback_errors.is_empty() {
                    return Err(error)
                        .context("install bundled skills; earlier installations were rolled back");
                }
                bail!(
                    "install bundled skills failed: {error:#}; rollback also failed: {}",
                    rollback_errors.join("; ")
                );
            }
        }
    }
    Ok(AgentSkillInstallSetReport { installations })
}

#[derive(Clone, Debug)]
struct InstallSnapshot {
    destination: PathBuf,
    destination_content: Option<Vec<u8>>,
    destination_permissions: Option<fs::Permissions>,
    state_path: PathBuf,
    state_content: Option<Vec<u8>>,
    state_permissions: Option<fs::Permissions>,
}

impl InstallSnapshot {
    fn capture(destination: &Path, state_path: &Path) -> Result<Self> {
        Ok(Self {
            destination: destination.to_path_buf(),
            destination_content: read_optional_bytes(destination)?,
            destination_permissions: optional_permissions(destination)?,
            state_path: state_path.to_path_buf(),
            state_content: read_optional_bytes(state_path)?,
            state_permissions: optional_permissions(state_path)?,
        })
    }

    fn restore(&self) -> Result<()> {
        let expected_state = render_state(&self.destination)?;
        restore_installed_file(
            &self.destination,
            self.destination_content.as_deref(),
            self.destination_permissions.as_ref(),
            BUNDLED_AGENT_SKILL.as_bytes(),
        )?;
        restore_installed_file(
            &self.state_path,
            self.state_content.as_deref(),
            self.state_permissions.as_ref(),
            &expected_state,
        )?;
        if self.destination_content.is_none() {
            let skill_dir = self
                .destination
                .parent()
                .context("bundled agent skill destination has no parent")?;
            match fs::remove_dir(skill_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("remove new skill directory {}", skill_dir.display())
                    });
                }
            }
        }
        Ok(())
    }
}

fn rollback_install_set(
    installations: &[AgentSkillInstallReport],
    snapshots: &[InstallSnapshot],
) -> Vec<String> {
    installations
        .iter()
        .zip(snapshots)
        .rev()
        .filter(|(report, _)| report.action != AgentSkillInstallAction::Unchanged)
        .filter_map(|(_, snapshot)| {
            snapshot.restore().err().map(|error| {
                format!(
                    "restore bundled skill {}: {error:#}",
                    snapshot.destination.display()
                )
            })
        })
        .collect()
}

fn restore_installed_file(
    path: &Path,
    original: Option<&[u8]>,
    original_permissions: Option<&fs::Permissions>,
    installed: &[u8],
) -> Result<()> {
    match original {
        Some(original) => {
            replace_file_if_unchanged(path, installed, original, original_permissions)?;
        }
        None => {
            remove_target_if_unchanged(path, &sha256(installed))?;
        }
    }
    Ok(())
}

fn optional_permissions(path: &Path) -> Result<Option<fs::Permissions>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn install_agent_skill_at(
    paths: &AgentPaths,
    destination: PathBuf,
    state_path: PathBuf,
    options: AgentSkillInstallOptions,
) -> Result<AgentSkillInstallReport> {
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
            replace_file_with_backup_if_unchanged(
                &backup_root,
                &paths.home,
                &destination,
                original_skill.as_deref(),
                BUNDLED_AGENT_SKILL.as_bytes(),
            )?
        }
        AgentSkillInstallAction::Unchanged => unreachable!("handled above"),
    };

    if let Err(error) = write_state(&state_path, &destination) {
        let mut rollback_errors = Vec::new();
        let installed_sha256 = sha256(BUNDLED_AGENT_SKILL.as_bytes());
        match action {
            AgentSkillInstallAction::Add => {
                if let Err(rollback_error) =
                    remove_target_if_unchanged(&destination, &installed_sha256)
                {
                    rollback_errors.push(format!(
                        "remove new skill {}: {rollback_error:#}",
                        destination.display()
                    ));
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
                if original_skill.is_some() {
                    let restore = backup
                        .as_deref()
                        .context("updated bundled skill backup was not recorded")
                        .and_then(|backup| {
                            restore_backup_atomically_if_unchanged(
                                backup,
                                &destination,
                                &installed_sha256,
                            )
                        });
                    if let Err(rollback_error) = restore {
                        rollback_errors.push(format!(
                            "restore skill {}: {rollback_error:#}",
                            destination.display()
                        ));
                    }
                }
            }
            AgentSkillInstallAction::Unchanged => unreachable!("handled above"),
        }
        match read_optional_bytes(&state_path) {
            Ok(current) if current == original_state => {}
            Ok(_) => rollback_errors.push(format!(
                "refusing to restore skill state {} because it changed concurrently",
                state_path.display()
            )),
            Err(rollback_error) => rollback_errors.push(format!(
                "inspect skill state {}: {rollback_error:#}",
                state_path.display()
            )),
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

pub fn claude_skill_destination(paths: &AgentPaths) -> PathBuf {
    paths
        .claude_home
        .join("skills")
        .join(BUNDLED_AGENT_SKILL_NAME)
        .join("SKILL.md")
}

pub fn claude_skill_state_path(paths: &AgentPaths) -> PathBuf {
    paths
        .home
        .join(".agent-sync")
        .join("state")
        .join("bundled-skill-claude.json")
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
    write_atomic(path, &render_state(destination)?)
}

fn render_state(destination: &Path) -> Result<Vec<u8>> {
    let state = BundledSkillState {
        schema_version: STATE_SCHEMA_VERSION,
        destination: destination.to_path_buf(),
        installed_sha256: sha256(BUNDLED_AGENT_SKILL.as_bytes()),
    };
    Ok([serde_json::to_vec_pretty(&state)?, b"\n".to_vec()].concat())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_install_rolls_back_an_earlier_destination() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let mut attempt = 0;

        let error = install_agent_skills_with(
            &paths,
            AgentSkillInstallOptions { dry_run: false },
            |paths, destination, state_path, options| {
                attempt += 1;
                if attempt == 2 {
                    bail!("injected second destination failure");
                }
                install_agent_skill_at(paths, destination, state_path, options)
            },
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("earlier installations were rolled back"));
        assert!(!skill_destination(&paths).exists());
        assert!(!skill_state_path(&paths).exists());
        assert!(!claude_skill_destination(&paths).exists());
        assert!(!claude_skill_state_path(&paths).exists());
    }
}

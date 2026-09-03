use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    adapters::{AgentKind, AgentPaths},
    fsx::{
        ensure_dir, hash_bytes, hash_path, path_content_equal, read_to_string_if_exists,
        remove_target_if_unchanged, replace_dir_with_backup_if_unchanged,
        replace_file_with_backup_if_unchanged, restore_backup_atomically_if_unchanged,
    },
    manifest::{Manifest, Resource, ResourceKind},
    mcp::{
        claude_mcp_server, claude_mcp_value, cursor_mcp_server, cursor_mcp_value,
        discover_cursor_project_mcp_names, ensure_cursor_mcp_write_safe, load_pack_mcp,
        read_claude_mcp_snapshot, read_cursor_mcp_snapshot,
        render_claude_mcp_additive_with_updates, render_cursor_mcp_additive_with_updates,
        write_codex_mcp, McpServer,
    },
    ownership::ClaudeResourceOwnership,
};

const MCP_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyOptions {
    pub dry_run: bool,
    pub backup_root: Option<PathBuf>,
    pub allow_updates: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Change {
    pub action: ChangeAction,
    pub target: AgentKind,
    pub resource: String,
    pub destination: PathBuf,
    pub backup: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeAction {
    Add,
    Update,
    ManagedUpdate,
    Unchanged,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyReport {
    pub dry_run: bool,
    pub changes: Vec<Change>,
    pub backup_root: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct AppliedWrite {
    destination: PathBuf,
    backup: Option<PathBuf>,
    installed_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct McpOwnershipState {
    schema_version: u32,
    destination: PathBuf,
    servers: BTreeMap<String, String>,
}

struct McpStateSnapshot {
    target: AgentKind,
    path: PathBuf,
    raw: Option<Vec<u8>>,
    state: McpOwnershipState,
}

#[derive(Default)]
struct McpApplyResult {
    config_write: Option<AppliedWrite>,
    state_write: Option<AppliedWrite>,
}

impl ApplyReport {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if self.dry_run {
            out.push_str("Dry run. No files written.\n");
        } else if let Some(root) = &self.backup_root {
            out.push_str(&format!("Applied changes. Backups: {}\n", root.display()));
        }
        out.push_str(&format_diff(&self.changes));
        out
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifyReport {
    pub ok: bool,
    pub checks: Vec<String>,
    pub errors: Vec<String>,
}

impl VerifyReport {
    pub fn to_text(&self) -> String {
        let mut out = if self.ok {
            "Verification passed.\n".to_string()
        } else {
            "Verification failed.\n".to_string()
        };
        for check in &self.checks {
            out.push_str(&format!("ok: {check}\n"));
        }
        for error in &self.errors {
            out.push_str(&format!("error: {error}\n"));
        }
        out
    }
}

pub fn diff_pack(paths: &AgentPaths, pack: &Path, targets: &[AgentKind]) -> Result<Vec<Change>> {
    diff_pack_with_policy(paths, pack, targets, true)
}

pub(crate) fn diff_pack_with_policy(
    paths: &AgentPaths,
    pack: &Path,
    targets: &[AgentKind],
    allow_updates: bool,
) -> Result<Vec<Change>> {
    let manifest = Manifest::load(pack)?;
    let mcp = load_pack_mcp(pack)?;
    let claude_ownership = if manifest.resources.iter().any(|resource| {
        matches!(resource.kind, ResourceKind::Skill | ResourceKind::Rule)
            && resource.targets.contains(&AgentKind::Claude)
            && targets.contains(&AgentKind::Claude)
    }) {
        Some(ClaudeResourceOwnership::load(paths)?)
    } else {
        None
    };
    let mut changes = Vec::new();
    for resource in &manifest.resources {
        for target in targets {
            if !resource.targets.contains(target) {
                continue;
            }
            let Some(destination) = destination_for(paths, resource, *target) else {
                continue;
            };
            let action = match resource.kind {
                ResourceKind::Mcp => {
                    mcp_change_action(paths, &mcp, &resource.name, *target, allow_updates)?
                }
                ResourceKind::Rule => rule_change_action(
                    pack,
                    resource,
                    *target,
                    &destination,
                    claude_ownership.as_ref(),
                    allow_updates,
                )?,
                _ => file_change_action(
                    pack,
                    resource,
                    *target,
                    &destination,
                    claude_ownership.as_ref(),
                    allow_updates,
                )?,
            };
            changes.push(Change {
                action,
                target: *target,
                resource: resource_label(resource),
                destination,
                backup: None,
            });
        }
    }
    Ok(changes)
}

pub fn apply_pack(
    paths: &AgentPaths,
    pack: &Path,
    targets: &[AgentKind],
    options: ApplyOptions,
) -> Result<ApplyReport> {
    let mut changes = diff_pack_with_policy(paths, pack, targets, options.allow_updates)?;
    if options.dry_run {
        return Ok(ApplyReport {
            dry_run: true,
            changes,
            backup_root: None,
        });
    }

    if !changes.iter().any(|change| {
        matches!(
            change.action,
            ChangeAction::Add | ChangeAction::Update | ChangeAction::ManagedUpdate
        )
    }) {
        return Ok(ApplyReport {
            dry_run: false,
            changes,
            backup_root: None,
        });
    }

    let backup_root = options.backup_root.unwrap_or_else(|| {
        paths
            .home
            .join(".agent-sync")
            .join("backups")
            .join(Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string())
    });
    ensure_dir(&backup_root)?;

    let manifest = Manifest::load(pack)?;
    let mcp = load_pack_mcp(pack)?;
    let mut claude_ownership = if manifest.resources.iter().any(|resource| {
        matches!(resource.kind, ResourceKind::Skill | ResourceKind::Rule)
            && resource.targets.contains(&AgentKind::Claude)
            && targets.contains(&AgentKind::Claude)
    }) {
        Some(ClaudeResourceOwnership::load(paths)?)
    } else {
        None
    };
    let mut claude_ownership_changed = false;
    let mut applied_mcp_targets = BTreeSet::new();
    let mut applied_writes = Vec::new();
    let apply_result = (|| -> Result<()> {
        for resource in &manifest.resources {
            for target in targets {
                if !resource.targets.contains(target) {
                    continue;
                }
                let action = if resource.kind != ResourceKind::Mcp {
                    let Some(destination) = destination_for(paths, resource, *target) else {
                        continue;
                    };
                    let action = match resource.kind {
                        ResourceKind::Rule => rule_change_action(
                            pack,
                            resource,
                            *target,
                            &destination,
                            claude_ownership.as_ref(),
                            options.allow_updates,
                        )?,
                        ResourceKind::Skill
                        | ResourceKind::MemoryReference
                        | ResourceKind::AutomationTemplate => file_change_action(
                            pack,
                            resource,
                            *target,
                            &destination,
                            claude_ownership.as_ref(),
                            options.allow_updates,
                        )?,
                        ResourceKind::Mcp => unreachable!(),
                    };
                    update_action_and_destination(
                        &mut changes,
                        *target,
                        resource,
                        action,
                        destination,
                    );
                    Some(action)
                } else {
                    planned_action(&changes, *target, resource)
                };
                if action == Some(ChangeAction::Update) && !options.allow_updates {
                    bail!(
                        "apply is blocked because {} {} became an update and target replacements are disabled",
                        target,
                        resource_label(resource)
                    );
                }
                if matches!(action, Some(ChangeAction::Skip | ChangeAction::Unchanged)) {
                    continue;
                }
                match resource.kind {
                    ResourceKind::Skill
                    | ResourceKind::MemoryReference
                    | ResourceKind::AutomationTemplate => {
                        let Some(destination) = destination_for(paths, resource, *target) else {
                            continue;
                        };
                        let source = pack.join(&resource.pack_path);
                        if path_content_equal(&source, &destination)? {
                            continue;
                        }
                        let installed_sha256 = hash_path(&source)?;
                        let latest_action = file_change_action(
                            pack,
                            resource,
                            *target,
                            &destination,
                            claude_ownership.as_ref(),
                            options.allow_updates,
                        )?;
                        update_action_and_destination(
                            &mut changes,
                            *target,
                            resource,
                            latest_action,
                            destination.clone(),
                        );
                        if latest_action == ChangeAction::Update && !options.allow_updates {
                            bail!(
                                "apply is blocked because {} {} became an update and target replacements are disabled",
                                target,
                                resource_label(resource)
                            );
                        }
                        if matches!(latest_action, ChangeAction::Skip | ChangeAction::Unchanged) {
                            continue;
                        }
                        let expected_sha256 = match latest_action {
                            ChangeAction::Add => None,
                            ChangeAction::Update | ChangeAction::ManagedUpdate => {
                                Some(hash_path(&destination)?)
                            }
                            ChangeAction::Skip | ChangeAction::Unchanged => unreachable!(),
                        };
                        let root = agent_root(paths, *target);
                        let backup = replace_dir_with_backup_if_unchanged(
                            &backup_root,
                            root,
                            &source,
                            &destination,
                            expected_sha256.as_deref(),
                        )?;
                        applied_writes.push(AppliedWrite {
                            destination: destination.clone(),
                            backup: backup.clone(),
                            installed_sha256: Some(installed_sha256.clone()),
                        });
                        if *target == AgentKind::Claude && resource.kind == ResourceKind::Skill {
                            claude_ownership
                                .as_mut()
                                .context("Claude resource ownership state was not loaded")?
                                .record(resource, destination.clone(), installed_sha256)?;
                            claude_ownership_changed = true;
                        }
                        update_backup(&mut changes, *target, resource, backup);
                    }
                    ResourceKind::Rule => {
                        let Some(destination) = destination_for(paths, resource, *target) else {
                            continue;
                        };
                        let content = rendered_rule(pack, resource, *target)?;
                        if read_to_string_if_exists(&destination)?.as_deref()
                            == Some(content.as_str())
                        {
                            continue;
                        }
                        let installed_sha256 = hash_bytes(content.as_bytes());
                        let latest_action = rule_change_action(
                            pack,
                            resource,
                            *target,
                            &destination,
                            claude_ownership.as_ref(),
                            options.allow_updates,
                        )?;
                        update_action_and_destination(
                            &mut changes,
                            *target,
                            resource,
                            latest_action,
                            destination.clone(),
                        );
                        if latest_action == ChangeAction::Update && !options.allow_updates {
                            bail!(
                                "apply is blocked because {} {} became an update and target replacements are disabled",
                                target,
                                resource_label(resource)
                            );
                        }
                        if matches!(latest_action, ChangeAction::Skip | ChangeAction::Unchanged) {
                            continue;
                        }
                        let expected = match latest_action {
                            ChangeAction::Add => None,
                            ChangeAction::Update | ChangeAction::ManagedUpdate => {
                                read_bytes_if_exists(&destination)?
                            }
                            ChangeAction::Skip | ChangeAction::Unchanged => unreachable!(),
                        };
                        let root = agent_root(paths, *target);
                        let backup = replace_file_with_backup_if_unchanged(
                            &backup_root,
                            root,
                            &destination,
                            expected.as_deref(),
                            content.as_bytes(),
                        )?;
                        applied_writes.push(AppliedWrite {
                            destination: destination.clone(),
                            backup: backup.clone(),
                            installed_sha256: Some(installed_sha256.clone()),
                        });
                        if *target == AgentKind::Claude {
                            claude_ownership
                                .as_mut()
                                .context("Claude resource ownership state was not loaded")?
                                .record(resource, destination.clone(), installed_sha256)?;
                            claude_ownership_changed = true;
                        }
                        update_backup(&mut changes, *target, resource, backup);
                    }
                    ResourceKind::Mcp => {
                        if applied_mcp_targets.contains(target) {
                            continue;
                        }
                        refresh_mcp_actions(
                            paths,
                            &manifest,
                            &mcp,
                            *target,
                            &mut changes,
                            options.allow_updates,
                        )?;
                        if !options.allow_updates
                            && changes.iter().any(|change| {
                                change.target == *target
                                    && change.resource.starts_with("Mcp:")
                                    && change.action == ChangeAction::Update
                            })
                        {
                            bail!(
                                "apply is blocked because a {} MCP server became an update and target replacements are disabled",
                                target
                            );
                        }
                        applied_mcp_targets.insert(*target);
                        if !mcp_needs_apply(&changes, *target) {
                            continue;
                        }
                        let result = apply_mcp(paths, &backup_root, &mcp, *target, &changes)?;
                        let backup = result
                            .config_write
                            .as_ref()
                            .and_then(|write| write.backup.clone());
                        if let Some(write) = result.config_write {
                            applied_writes.push(write);
                        }
                        if let Some(write) = result.state_write {
                            applied_writes.push(write);
                        }
                        update_mcp_backup(&mut changes, *target, backup);
                    }
                }
            }
        }
        if claude_ownership_changed {
            let ownership = claude_ownership
                .as_ref()
                .context("Claude resource ownership state was not loaded")?;
            let content = ownership.render()?;
            let backup = replace_file_with_backup_if_unchanged(
                &backup_root,
                &paths.home,
                ownership.path(),
                ownership.raw(),
                &content,
            )
            .context("write Claude resource ownership state")?;
            applied_writes.push(AppliedWrite {
                destination: ownership.path().to_path_buf(),
                backup,
                installed_sha256: Some(hash_bytes(&content)),
            });
        }
        let verification = verify_pack_with_policy(paths, pack, targets, options.allow_updates)?;
        if !verification.ok {
            bail!(
                "post-apply verification failed: {}",
                verification.errors.join("; ")
            );
        }
        Ok(())
    })();

    if let Err(error) = apply_result {
        let rollback = rollback_applied_writes(&applied_writes);
        return match rollback {
            Ok(()) => Err(error).with_context(|| {
                format!(
                    "apply failed and all completed writes were rolled back; backups: {}",
                    backup_root.display()
                )
            }),
            Err(rollback_error) => Err(anyhow::anyhow!(
                "apply failed: {error:#}; rollback also failed: {rollback_error:#}; backups: {}",
                backup_root.display()
            )),
        };
    }

    Ok(ApplyReport {
        dry_run: false,
        changes,
        backup_root: Some(backup_root),
    })
}

pub fn verify_pack(paths: &AgentPaths, pack: &Path, targets: &[AgentKind]) -> Result<VerifyReport> {
    verify_pack_with_policy(paths, pack, targets, true)
}

pub(crate) fn verify_pack_with_policy(
    paths: &AgentPaths,
    pack: &Path,
    targets: &[AgentKind],
    allow_updates: bool,
) -> Result<VerifyReport> {
    let changes = diff_pack_with_policy(paths, pack, targets, allow_updates)?;
    let mut checks = Vec::new();
    let mut errors = Vec::new();

    for change in changes {
        match change.action {
            ChangeAction::Unchanged => checks.push(format!(
                "{} {} matches at {}",
                change.target,
                change.resource,
                change.destination.display()
            )),
            ChangeAction::Skip => checks.push(format!(
                "{} {} is target-owned and preserved at {}",
                change.target,
                change.resource,
                change.destination.display()
            )),
            ChangeAction::Add | ChangeAction::Update | ChangeAction::ManagedUpdate => {
                errors.push(format!(
                    "{} {} still needs {:?} at {}",
                    change.target,
                    change.resource,
                    change.action,
                    change.destination.display()
                ))
            }
        }
    }

    Ok(VerifyReport {
        ok: errors.is_empty(),
        checks,
        errors,
    })
}

pub fn format_diff(changes: &[Change]) -> String {
    if changes.is_empty() {
        return "No changes.\n".to_string();
    }
    let mut out = String::new();
    for change in changes {
        out.push_str(&format!(
            "{:?} {} {} -> {}\n",
            change.action,
            change.target,
            change.resource,
            change.destination.display()
        ));
    }
    out
}

fn destination_for(paths: &AgentPaths, resource: &Resource, target: AgentKind) -> Option<PathBuf> {
    match (resource.kind, target) {
        (ResourceKind::Skill, AgentKind::Codex) => {
            Some(paths.codex_home.join("skills").join(&resource.name))
        }
        (ResourceKind::Skill, AgentKind::Claude) => {
            Some(paths.claude_home.join("skills").join(&resource.name))
        }
        (ResourceKind::Skill, AgentKind::Cursor) => {
            let cursor_owned = paths.cursor_home.join("skills").join(&resource.name);
            if cursor_owned.exists() {
                return Some(cursor_owned);
            }
            let shared = match resource.source_agent.as_str() {
                "codex" => paths.codex_home.join("skills").join(&resource.name),
                "claude" => paths.claude_home.join("skills").join(&resource.name),
                "agents" => paths.agents_home.join("skills").join(&resource.name),
                _ => cursor_owned.clone(),
            };
            if shared.exists() {
                Some(shared)
            } else {
                Some(cursor_owned)
            }
        }
        (ResourceKind::Rule, AgentKind::Codex) if resource.name == "codex-agents" => {
            Some(paths.codex_home.join("AGENTS.md"))
        }
        (ResourceKind::Rule, AgentKind::Claude) if resource.name == "codex-agents" => {
            let imported = paths
                .claude_home
                .join("rules")
                .join("imported-codex-agents.md");
            let legacy = paths
                .claude_home
                .join("rules")
                .join("codex-global-agent-rules.md");
            Some(if fs::symlink_metadata(&imported).is_ok() {
                imported
            } else if fs::symlink_metadata(&legacy).is_ok() {
                legacy
            } else {
                imported
            })
        }
        (ResourceKind::Rule, AgentKind::Claude) if resource.name == "claude-user" => {
            Some(paths.claude_home.join("CLAUDE.md"))
        }
        (ResourceKind::Rule, AgentKind::Cursor) if resource.name == "codex-agents" => Some(
            paths
                .cursor_home
                .join("rules")
                .join("imported-codex-agents.mdc"),
        ),
        (ResourceKind::Mcp, AgentKind::Codex) => Some(paths.codex_home.join("config.toml")),
        (ResourceKind::Mcp, AgentKind::Claude) => Some(paths.claude_config.clone()),
        (ResourceKind::Mcp, AgentKind::Cursor) => Some(paths.cursor_config.clone()),
        (ResourceKind::MemoryReference, AgentKind::Claude) => Some(
            paths
                .claude_home
                .join("agent-sync-import")
                .join(&resource.name),
        ),
        _ => None,
    }
}

fn file_change_action(
    pack: &Path,
    resource: &Resource,
    target: AgentKind,
    destination: &Path,
    claude_ownership: Option<&ClaudeResourceOwnership>,
    allow_updates: bool,
) -> Result<ChangeAction> {
    let source = pack.join(&resource.pack_path);
    let metadata = symlink_metadata_if_exists(destination)?;
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
        && matches!(target, AgentKind::Claude | AgentKind::Cursor)
    {
        Ok(ChangeAction::Skip)
    } else if metadata.is_none() {
        Ok(ChangeAction::Add)
    } else if path_content_equal(&source, destination)? {
        Ok(ChangeAction::Unchanged)
    } else if target == AgentKind::Cursor {
        Ok(ChangeAction::Skip)
    } else if target == AgentKind::Claude && resource.kind == ResourceKind::Skill {
        claude_replacement_action(resource, destination, claude_ownership, allow_updates)
    } else {
        Ok(ChangeAction::Update)
    }
}

fn rule_change_action(
    pack: &Path,
    resource: &Resource,
    target: AgentKind,
    destination: &Path,
    claude_ownership: Option<&ClaudeResourceOwnership>,
    allow_updates: bool,
) -> Result<ChangeAction> {
    let content = rendered_rule(pack, resource, target)?;
    let metadata = symlink_metadata_if_exists(destination)?;
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
        && matches!(target, AgentKind::Claude | AgentKind::Cursor)
    {
        Ok(ChangeAction::Skip)
    } else if metadata.is_none() {
        Ok(ChangeAction::Add)
    } else if read_to_string_if_exists(destination)?.as_deref() == Some(content.as_str()) {
        Ok(ChangeAction::Unchanged)
    } else if target == AgentKind::Cursor {
        let existing = read_to_string_if_exists(destination)?;
        Ok(cursor_rule_replacement_action(
            resource,
            existing.as_deref(),
        ))
    } else if target == AgentKind::Claude {
        claude_replacement_action(resource, destination, claude_ownership, allow_updates)
    } else {
        Ok(ChangeAction::Update)
    }
}

fn claude_replacement_action(
    resource: &Resource,
    destination: &Path,
    ownership: Option<&ClaudeResourceOwnership>,
    allow_updates: bool,
) -> Result<ChangeAction> {
    let ownership = ownership.context("Claude resource ownership state was not loaded")?;
    let current_sha256 = hash_path(destination)?;
    if ownership.is_unmodified(resource, destination, &current_sha256)? {
        Ok(ChangeAction::ManagedUpdate)
    } else if allow_updates {
        Ok(ChangeAction::Update)
    } else {
        Ok(ChangeAction::Skip)
    }
}

fn cursor_rule_replacement_action(resource: &Resource, existing: Option<&str>) -> ChangeAction {
    if resource.name == "codex-agents"
        && existing.is_some_and(|content| {
            content == LEGACY_CURSOR_CODEX_RULE || managed_cursor_rule_is_unmodified(content)
        })
    {
        ChangeAction::ManagedUpdate
    } else {
        ChangeAction::Skip
    }
}

fn mcp_change_action(
    paths: &AgentPaths,
    mcp: &BTreeMap<String, McpServer>,
    name: &str,
    target: AgentKind,
    allow_updates: bool,
) -> Result<ChangeAction> {
    let Some(server) = mcp.get(name) else {
        bail!("manifest MCP `{name}` is missing from mcp/servers.json");
    };
    let existing = match target {
        AgentKind::Codex => crate::mcp::discover_codex_mcp(&paths.codex_home.join("config.toml"))?,
        AgentKind::Claude => {
            let (_, configured) = read_claude_mcp_snapshot(&paths.claude_config)?;
            if let Some(existing) = configured.get(name) {
                if claude_mcp_server(existing).as_ref() == Some(server) {
                    return Ok(ChangeAction::Unchanged);
                }
                let state = load_mcp_state(paths, AgentKind::Claude)?;
                let current_sha256 = mcp_value_sha256(existing)?;
                return Ok(if state.state.servers.get(name) == Some(&current_sha256) {
                    ChangeAction::ManagedUpdate
                } else if allow_updates {
                    ChangeAction::Update
                } else {
                    ChangeAction::Skip
                });
            }
            return Ok(ChangeAction::Add);
        }
        AgentKind::Cursor => {
            let project_owned = discover_cursor_project_mcp_names(&paths.cursor_home)?;
            if project_owned.contains(name) {
                return Ok(ChangeAction::Skip);
            }
            let (_, configured) = read_cursor_mcp_snapshot(&paths.cursor_config)?;
            if let Some(existing) = configured.get(name) {
                if cursor_mcp_server(existing).as_ref() == Some(server) {
                    return Ok(ChangeAction::Unchanged);
                }
                let state = load_mcp_state(paths, AgentKind::Cursor)?;
                let current_sha256 = mcp_value_sha256(existing)?;
                return Ok(if state.state.servers.get(name) == Some(&current_sha256) {
                    ChangeAction::ManagedUpdate
                } else {
                    ChangeAction::Skip
                });
            }
            ensure_cursor_mcp_write_safe(&paths.cursor_config)?;
            return Ok(ChangeAction::Add);
        }
    };
    Ok(match existing.get(name) {
        Some(existing) if existing == server => ChangeAction::Unchanged,
        Some(_) => ChangeAction::Update,
        None => ChangeAction::Add,
    })
}

fn symlink_metadata_if_exists(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

const CURSOR_CODEX_RULE_FRONTMATTER: &str =
    "---\ndescription: Imported Codex agent guidance\nalwaysApply: true\n---\n";
const CURSOR_CODEX_RULE_MARKER_PREFIX: &str =
    "<!-- agent-sync-managed: cursor-codex-agents body-sha256=";
const CURSOR_CODEX_RULE_MARKER_SUFFIX: &str = " -->";
const LEGACY_CURSOR_CODEX_RULE: &str = "---\ndescription: Bridge to Codex global agent rules\nalwaysApply: true\n---\n# Codex Agent Rule Bridge\n\nBefore starting a task, read and follow `~/.codex/AGENTS.md` when it exists. Treat it as shared guidance. Direct user instructions and Cursor-specific settings and rules take precedence if they conflict with that file.\n\nWhen prior work may matter, search the QMD `sessions` collection. QMD history is searchable context, not a resumable Cursor chat.\n";

fn rendered_rule(pack: &Path, resource: &Resource, target: AgentKind) -> Result<String> {
    let raw = std::fs::read_to_string(pack.join(&resource.pack_path))?;
    match (resource.name.as_str(), target) {
        ("codex-agents", AgentKind::Claude) => Ok(format!(
            "# Imported Codex Agent Rules\n\nImported by `agent-sync` from pack resource `codex-agents`.\n\n{}",
            raw
        )),
        ("codex-agents", AgentKind::Cursor) => Ok(render_cursor_codex_rule(&raw)),
        _ => Ok(raw),
    }
}

fn render_cursor_codex_rule(raw: &str) -> String {
    let body = format!(
        "# Imported Codex Agent Rules\n\nImported by `agent-sync` from pack resource `codex-agents`. Cursor-specific settings and rules take precedence if they conflict.\n\n{raw}"
    );
    let body_sha256 = hash_bytes(body.as_bytes());
    format!(
        "{CURSOR_CODEX_RULE_FRONTMATTER}{CURSOR_CODEX_RULE_MARKER_PREFIX}{body_sha256}{CURSOR_CODEX_RULE_MARKER_SUFFIX}\n{body}"
    )
}

fn managed_cursor_rule_is_unmodified(content: &str) -> bool {
    let Some(marked_body) = content.strip_prefix(CURSOR_CODEX_RULE_FRONTMATTER) else {
        return false;
    };
    let Some((marker, body)) = marked_body.split_once('\n') else {
        return false;
    };
    let Some(expected_sha256) = cursor_rule_marker_hash(marker) else {
        return false;
    };
    hash_bytes(body.as_bytes()) == expected_sha256
}

fn cursor_rule_marker_hash(marker: &str) -> Option<&str> {
    let hash = marker
        .strip_prefix(CURSOR_CODEX_RULE_MARKER_PREFIX)?
        .strip_suffix(CURSOR_CODEX_RULE_MARKER_SUFFIX)?;
    (hash.len() == 64
        && hash
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, 'a'..='f')))
    .then_some(hash)
}

fn apply_mcp(
    paths: &AgentPaths,
    backup_root: &Path,
    mcp: &BTreeMap<String, McpServer>,
    target: AgentKind,
    changes: &[Change],
) -> Result<McpApplyResult> {
    match target {
        AgentKind::Claude => apply_claude_mcp(paths, backup_root, mcp, changes),
        AgentKind::Codex => {
            let path = paths.codex_home.join("config.toml");
            let expected = read_bytes_if_exists(&path)?;
            let content = write_codex_mcp(&path, mcp)?;
            if expected.as_deref() == Some(content.as_slice()) {
                return Ok(McpApplyResult::default());
            }
            let installed_sha256 = hash_bytes(&content);
            let backup = replace_file_with_backup_if_unchanged(
                backup_root,
                &paths.codex_home,
                &path,
                expected.as_deref(),
                &content,
            )?;
            Ok(McpApplyResult {
                config_write: Some(AppliedWrite {
                    destination: path,
                    backup,
                    installed_sha256: Some(installed_sha256),
                }),
                state_write: None,
            })
        }
        AgentKind::Cursor => apply_cursor_mcp(paths, backup_root, mcp, changes),
    }
}

fn apply_claude_mcp(
    paths: &AgentPaths,
    backup_root: &Path,
    mcp: &BTreeMap<String, McpServer>,
    changes: &[Change],
) -> Result<McpApplyResult> {
    let mut state_snapshot = load_mcp_state(paths, AgentKind::Claude)?;
    let (expected_config, configured) = read_claude_mcp_snapshot(&paths.claude_config)?;
    let mut servers = BTreeMap::new();
    let mut replacements = BTreeSet::new();

    for (name, action) in mcp_actions(changes, AgentKind::Claude) {
        let server = mcp
            .get(&name)
            .with_context(|| format!("manifest MCP `{name}` is missing from mcp/servers.json"))?;
        let eligible = match action {
            ChangeAction::Add => !configured.contains_key(&name),
            ChangeAction::ManagedUpdate => configured.get(&name).is_some_and(|current| {
                mcp_value_sha256(current)
                    .ok()
                    .as_ref()
                    .is_some_and(|current_sha256| {
                        state_snapshot.state.servers.get(&name) == Some(current_sha256)
                    })
            }),
            ChangeAction::Update => configured.contains_key(&name),
            ChangeAction::Unchanged | ChangeAction::Skip => false,
        };
        if !eligible {
            continue;
        }
        if matches!(action, ChangeAction::Update | ChangeAction::ManagedUpdate) {
            replacements.insert(name.clone());
        }
        let installed_sha256 = mcp_value_sha256(&claude_mcp_value(server)?)?;
        state_snapshot
            .state
            .servers
            .insert(name.clone(), installed_sha256);
        servers.insert(name, server.clone());
    }

    if servers.is_empty() {
        return Ok(McpApplyResult::default());
    }

    let content = render_claude_mcp_additive_with_updates(
        &paths.claude_config,
        expected_config.as_deref(),
        &servers,
        &replacements,
    )?;
    if expected_config.as_deref() == Some(content.as_slice()) {
        return Ok(McpApplyResult::default());
    }
    let config_backup = replace_file_with_backup_if_unchanged(
        backup_root,
        &paths.home,
        &paths.claude_config,
        expected_config.as_deref(),
        &content,
    )?;
    let config_write = AppliedWrite {
        destination: paths.claude_config.clone(),
        backup: config_backup,
        installed_sha256: Some(hash_bytes(&content)),
    };
    let state_write =
        write_mcp_state_with_rollback(paths, backup_root, &state_snapshot, &config_write)?;

    Ok(McpApplyResult {
        config_write: Some(config_write),
        state_write,
    })
}

fn apply_cursor_mcp(
    paths: &AgentPaths,
    backup_root: &Path,
    mcp: &BTreeMap<String, McpServer>,
    changes: &[Change],
) -> Result<McpApplyResult> {
    let mut state_snapshot = load_mcp_state(paths, AgentKind::Cursor)?;
    let (expected_config, configured) = read_cursor_mcp_snapshot(&paths.cursor_config)?;
    let project_owned = discover_cursor_project_mcp_names(&paths.cursor_home)?;
    let mut servers = BTreeMap::new();
    let mut managed_updates = BTreeSet::new();

    for (name, action) in mcp_actions(changes, AgentKind::Cursor) {
        let server = mcp
            .get(&name)
            .with_context(|| format!("manifest MCP `{name}` is missing from mcp/servers.json"))?;
        if project_owned.contains(&name) {
            continue;
        }
        let eligible = match action {
            ChangeAction::Add => !configured.contains_key(&name),
            ChangeAction::ManagedUpdate => configured.get(&name).is_some_and(|current| {
                mcp_value_sha256(current)
                    .ok()
                    .as_ref()
                    .is_some_and(|current_sha256| {
                        state_snapshot.state.servers.get(&name) == Some(current_sha256)
                    })
            }),
            ChangeAction::Update | ChangeAction::Unchanged | ChangeAction::Skip => false,
        };
        if !eligible {
            continue;
        }
        if action == ChangeAction::ManagedUpdate {
            managed_updates.insert(name.clone());
        }
        let installed_sha256 = mcp_value_sha256(&cursor_mcp_value(server)?)?;
        state_snapshot
            .state
            .servers
            .insert(name.clone(), installed_sha256);
        servers.insert(name, server.clone());
    }

    if servers.is_empty() {
        return Ok(McpApplyResult::default());
    }

    let content = render_cursor_mcp_additive_with_updates(
        &paths.cursor_config,
        expected_config.as_deref(),
        &servers,
        &managed_updates,
    )?;
    if expected_config.as_deref() == Some(content.as_slice()) {
        return Ok(McpApplyResult::default());
    }
    let config_installed_sha256 = hash_bytes(&content);
    let config_backup = replace_file_with_backup_if_unchanged(
        backup_root,
        &paths.cursor_home,
        &paths.cursor_config,
        expected_config.as_deref(),
        &content,
    )?;
    let config_write = AppliedWrite {
        destination: paths.cursor_config.clone(),
        backup: config_backup,
        installed_sha256: Some(config_installed_sha256),
    };

    let state_write =
        write_mcp_state_with_rollback(paths, backup_root, &state_snapshot, &config_write)?;

    Ok(McpApplyResult {
        config_write: Some(config_write),
        state_write,
    })
}

fn mcp_actions(changes: &[Change], target: AgentKind) -> BTreeMap<String, ChangeAction> {
    changes
        .iter()
        .filter(|change| change.target == target)
        .filter_map(|change| {
            change
                .resource
                .strip_prefix("Mcp:")
                .map(|name| (name.to_string(), change.action))
        })
        .collect()
}

fn mcp_state_path(paths: &AgentPaths, target: AgentKind) -> Result<PathBuf> {
    let file = match target {
        AgentKind::Claude => "claude-mcp.json",
        AgentKind::Cursor => "cursor-mcp.json",
        AgentKind::Codex => bail!("Codex MCP ownership is not tracked in a JSON state file"),
    };
    Ok(paths.home.join(".agent-sync").join("state").join(file))
}

fn load_mcp_state(paths: &AgentPaths, target: AgentKind) -> Result<McpStateSnapshot> {
    let path = mcp_state_path(paths, target)?;
    ensure_safe_mcp_state_path(&path, target)?;
    let raw = read_bytes_if_exists(&path)?;
    let state = match raw.as_deref() {
        Some(raw) => serde_json::from_slice(raw).with_context(|| {
            format!(
                "parse {} MCP ownership state {}",
                mcp_agent_name(target),
                path.display()
            )
        })?,
        None => McpOwnershipState {
            schema_version: MCP_STATE_SCHEMA_VERSION,
            destination: mcp_destination(paths, target),
            servers: BTreeMap::new(),
        },
    };
    validate_mcp_state(&state, paths, target, &path)?;
    Ok(McpStateSnapshot {
        target,
        path,
        raw,
        state,
    })
}

fn validate_mcp_state(
    state: &McpOwnershipState,
    paths: &AgentPaths,
    target: AgentKind,
    path: &Path,
) -> Result<()> {
    let agent = mcp_agent_name(target);
    if state.schema_version != MCP_STATE_SCHEMA_VERSION {
        bail!(
            "unsupported {agent} MCP ownership state version {} in {}",
            state.schema_version,
            path.display()
        );
    }
    let destination = mcp_destination(paths, target);
    if state.destination != destination {
        bail!(
            "refusing to use {agent} MCP ownership state for {}; it records {}",
            destination.display(),
            state.destination.display()
        );
    }
    if let Some((name, _)) = state.servers.iter().find(|(_, hash)| !valid_sha256(hash)) {
        bail!(
            "{agent} MCP ownership state for `{name}` has an invalid hash in {}",
            path.display()
        );
    }
    Ok(())
}

fn ensure_safe_mcp_state_path(path: &Path, target: AgentKind) -> Result<()> {
    let agent = mcp_agent_name(target);
    let Some(metadata) = symlink_metadata_if_exists(path)? else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to read or replace symlinked {agent} MCP ownership state {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "{agent} MCP ownership state path {} is not a regular file",
            path.display()
        );
    }
    Ok(())
}

fn render_mcp_state(state: &McpOwnershipState) -> Result<Vec<u8>> {
    Ok([serde_json::to_vec_pretty(state)?, b"\n".to_vec()].concat())
}

fn mcp_value_sha256(value: &Value) -> Result<String> {
    Ok(hash_bytes(&serde_json::to_vec(value)?))
}

fn write_mcp_state_with_rollback(
    paths: &AgentPaths,
    backup_root: &Path,
    snapshot: &McpStateSnapshot,
    config_write: &AppliedWrite,
) -> Result<Option<AppliedWrite>> {
    let state_content = render_mcp_state(&snapshot.state)?;
    if snapshot.raw.as_deref() == Some(state_content.as_slice()) {
        return Ok(None);
    }
    match replace_file_with_backup_if_unchanged(
        backup_root,
        &paths.home,
        &snapshot.path,
        snapshot.raw.as_deref(),
        &state_content,
    ) {
        Ok(backup) => Ok(Some(AppliedWrite {
            destination: snapshot.path.clone(),
            backup,
            installed_sha256: Some(hash_bytes(&state_content)),
        })),
        Err(error) => {
            let agent = mcp_agent_name(snapshot.target);
            let rollback = rollback_applied_writes(std::slice::from_ref(config_write));
            match rollback {
                Ok(()) => Err(error).with_context(|| {
                    format!("write {agent} MCP ownership state; MCP config was rolled back")
                }),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "write {agent} MCP ownership state failed: {error:#}; MCP config rollback also failed: {rollback_error:#}"
                )),
            }
        }
    }
}

fn mcp_agent_name(target: AgentKind) -> &'static str {
    match target {
        AgentKind::Claude => "Claude",
        AgentKind::Cursor => "Cursor",
        AgentKind::Codex => "Codex",
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_bytes_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn mcp_destination(paths: &AgentPaths, target: AgentKind) -> PathBuf {
    match target {
        AgentKind::Claude => paths.claude_config.clone(),
        AgentKind::Codex => paths.codex_home.join("config.toml"),
        AgentKind::Cursor => paths.cursor_config.clone(),
    }
}

fn rollback_applied_writes(writes: &[AppliedWrite]) -> Result<()> {
    let mut errors = Vec::new();
    for write in writes.iter().rev() {
        let current_metadata = match fs::symlink_metadata(&write.destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                errors.push(format!(
                    "refusing to restore {} because the installed path was removed concurrently",
                    write.destination.display()
                ));
                continue;
            }
            Err(error) => {
                errors.push(format!(
                    "inspect {} before rollback: {error}",
                    write.destination.display()
                ));
                continue;
            }
        };
        if current_metadata.file_type().is_symlink() {
            errors.push(format!(
                "refusing to roll back concurrently replaced symlink {}",
                write.destination.display()
            ));
            continue;
        }
        let Some(installed_sha256) = &write.installed_sha256 else {
            errors.push(format!(
                "refusing to roll back {} because its installed hash was not recorded",
                write.destination.display()
            ));
            continue;
        };
        match hash_path(&write.destination) {
            Ok(current) if &current == installed_sha256 => {}
            Ok(_) => {
                errors.push(format!(
                    "refusing to overwrite a concurrent edit at {} during rollback",
                    write.destination.display()
                ));
                continue;
            }
            Err(error) => {
                errors.push(format!(
                    "hash {} before rollback: {error:#}",
                    write.destination.display()
                ));
                continue;
            }
        }
        let Some(backup) = &write.backup else {
            if let Err(error) = remove_target_if_unchanged(&write.destination, installed_sha256) {
                errors.push(format!("remove {}: {error:#}", write.destination.display()));
            }
            continue;
        };
        if let Err(error) =
            restore_backup_atomically_if_unchanged(backup, &write.destination, installed_sha256)
        {
            errors.push(format!(
                "restore {} from {}: {error:#}",
                write.destination.display(),
                backup.display()
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

fn agent_root(paths: &AgentPaths, target: AgentKind) -> &Path {
    match target {
        AgentKind::Codex => &paths.codex_home,
        AgentKind::Claude => &paths.claude_home,
        AgentKind::Cursor => &paths.cursor_home,
    }
}

fn planned_action(
    changes: &[Change],
    target: AgentKind,
    resource: &Resource,
) -> Option<ChangeAction> {
    let label = resource_label(resource);
    changes
        .iter()
        .find(|change| change.target == target && change.resource == label)
        .map(|change| change.action)
}

fn update_action_and_destination(
    changes: &mut [Change],
    target: AgentKind,
    resource: &Resource,
    action: ChangeAction,
    destination: PathBuf,
) {
    let label = resource_label(resource);
    if let Some(change) = changes
        .iter_mut()
        .find(|change| change.target == target && change.resource == label)
    {
        change.action = action;
        change.destination = destination;
    }
}

fn refresh_mcp_actions(
    paths: &AgentPaths,
    manifest: &Manifest,
    mcp: &BTreeMap<String, McpServer>,
    target: AgentKind,
    changes: &mut [Change],
    allow_updates: bool,
) -> Result<()> {
    for resource in manifest
        .resources
        .iter()
        .filter(|resource| resource.kind == ResourceKind::Mcp && resource.targets.contains(&target))
    {
        let action = mcp_change_action(paths, mcp, &resource.name, target, allow_updates)?;
        let destination = mcp_destination(paths, target);
        update_action_and_destination(changes, target, resource, action, destination);
    }
    Ok(())
}

fn resource_label(resource: &Resource) -> String {
    format!("{:?}:{}", resource.kind, resource.name)
}

fn update_backup(
    changes: &mut [Change],
    target: AgentKind,
    resource: &Resource,
    backup: Option<PathBuf>,
) {
    let label = resource_label(resource);
    for change in changes {
        if change.target == target && change.resource == label {
            change.backup = backup.clone();
        }
    }
}

fn mcp_needs_apply(changes: &[Change], target: AgentKind) -> bool {
    changes.iter().any(|change| {
        change.target == target
            && change.resource.starts_with("Mcp:")
            && matches!(
                change.action,
                ChangeAction::Add | ChangeAction::Update | ChangeAction::ManagedUpdate
            )
    })
}

fn update_mcp_backup(changes: &mut [Change], target: AgentKind, backup: Option<PathBuf>) {
    for change in changes {
        if change.target == target
            && change.resource.starts_with("Mcp:")
            && matches!(
                change.action,
                ChangeAction::Add | ChangeAction::Update | ChangeAction::ManagedUpdate
            )
        {
            change.backup = backup.clone();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_rule_marker_only_accepts_unmodified_generated_content() {
        let generated = render_cursor_codex_rule("# Rules\n\nKeep changes scoped.\n");
        assert!(managed_cursor_rule_is_unmodified(&generated));

        let edited = format!("{generated}\nCursor-owned addition.\n");
        assert!(!managed_cursor_rule_is_unmodified(&edited));
    }

    #[test]
    fn exact_legacy_cursor_rule_is_a_managed_upgrade_only_while_unmodified() {
        let resource = Resource {
            kind: ResourceKind::Rule,
            name: "codex-agents".to_string(),
            source_agent: "codex".to_string(),
            pack_path: "rules/codex-agents.md".to_string(),
            sha256: String::new(),
            targets: vec![AgentKind::Cursor],
        };

        assert_eq!(
            cursor_rule_replacement_action(&resource, Some(LEGACY_CURSOR_CODEX_RULE)),
            ChangeAction::ManagedUpdate
        );
        let edited = format!("{LEGACY_CURSOR_CODEX_RULE}Cursor-owned addition.\n");
        assert_eq!(
            cursor_rule_replacement_action(&resource, Some(&edited)),
            ChangeAction::Skip
        );
    }
}

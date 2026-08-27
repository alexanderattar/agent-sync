use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    adapters::{AgentKind, AgentPaths},
    fsx::{
        ensure_dir, hash_bytes, hash_path, path_content_equal, read_to_string_if_exists,
        replace_dir_with_backup, replace_file_with_backup, restore_backup_atomically,
    },
    manifest::{Manifest, Resource, ResourceKind},
    mcp::{
        discover_cursor_mcp, discover_cursor_mcp_names, discover_cursor_project_mcp_names,
        ensure_cursor_mcp_write_safe, load_pack_mcp, write_claude_mcp, write_codex_mcp,
        write_cursor_mcp_additive, McpServer,
    },
};

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
    let manifest = Manifest::load(pack)?;
    let mcp = load_pack_mcp(pack)?;
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
                ResourceKind::Mcp => mcp_change_action(paths, &mcp, &resource.name, *target)?,
                ResourceKind::Rule => rule_change_action(pack, resource, *target, &destination)?,
                _ => file_change_action(pack, resource, *target, &destination)?,
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
    let mut changes = diff_pack(paths, pack, targets)?;
    if options.dry_run {
        return Ok(ApplyReport {
            dry_run: true,
            changes,
            backup_root: None,
        });
    }

    if !changes
        .iter()
        .any(|change| matches!(change.action, ChangeAction::Add | ChangeAction::Update))
    {
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
                        ResourceKind::Rule => {
                            rule_change_action(pack, resource, *target, &destination)?
                        }
                        ResourceKind::Skill
                        | ResourceKind::MemoryReference
                        | ResourceKind::AutomationTemplate => {
                            file_change_action(pack, resource, *target, &destination)?
                        }
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
                        let latest_action =
                            file_change_action(pack, resource, *target, &destination)?;
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
                        let root = agent_root(paths, *target);
                        let backup =
                            replace_dir_with_backup(&backup_root, root, &source, &destination)?;
                        applied_writes.push(AppliedWrite {
                            destination: destination.clone(),
                            backup: backup.clone(),
                            installed_sha256: Some(installed_sha256),
                        });
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
                        let latest_action =
                            rule_change_action(pack, resource, *target, &destination)?;
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
                        let root = agent_root(paths, *target);
                        let backup = replace_file_with_backup(
                            &backup_root,
                            root,
                            &destination,
                            content.as_bytes(),
                        )?;
                        applied_writes.push(AppliedWrite {
                            destination: destination.clone(),
                            backup: backup.clone(),
                            installed_sha256: Some(installed_sha256),
                        });
                        update_backup(&mut changes, *target, resource, backup);
                    }
                    ResourceKind::Mcp => {
                        if applied_mcp_targets.contains(target) {
                            continue;
                        }
                        refresh_mcp_actions(paths, &manifest, &mcp, *target, &mut changes)?;
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
                        let destination = mcp_destination(paths, *target);
                        let existed = destination.exists();
                        let (backup, installed_sha256) =
                            apply_mcp(paths, &backup_root, &mcp, *target)?;
                        if installed_sha256.is_some()
                            && (backup.is_some() || (!existed && destination.exists()))
                        {
                            applied_writes.push(AppliedWrite {
                                installed_sha256,
                                destination,
                                backup: backup.clone(),
                            });
                        }
                        update_mcp_backup(&mut changes, *target, backup);
                    }
                }
            }
        }
        let verification = verify_pack(paths, pack, targets)?;
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
    let changes = diff_pack(paths, pack, targets)?;
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
            ChangeAction::Add | ChangeAction::Update => errors.push(format!(
                "{} {} still needs {:?} at {}",
                change.target,
                change.resource,
                change.action,
                change.destination.display()
            )),
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
        (ResourceKind::Rule, AgentKind::Claude) if resource.name == "codex-agents" => Some(
            paths
                .claude_home
                .join("rules")
                .join("imported-codex-agents.md"),
        ),
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
) -> Result<ChangeAction> {
    let source = pack.join(&resource.pack_path);
    let metadata = symlink_metadata_if_exists(destination)?;
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
        && target == AgentKind::Cursor
    {
        Ok(ChangeAction::Skip)
    } else if metadata.is_none() {
        Ok(ChangeAction::Add)
    } else if path_content_equal(&source, destination)? {
        Ok(ChangeAction::Unchanged)
    } else if target == AgentKind::Cursor {
        Ok(ChangeAction::Skip)
    } else {
        Ok(ChangeAction::Update)
    }
}

fn rule_change_action(
    pack: &Path,
    resource: &Resource,
    target: AgentKind,
    destination: &Path,
) -> Result<ChangeAction> {
    let content = rendered_rule(pack, resource, target)?;
    let metadata = symlink_metadata_if_exists(destination)?;
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
        && target == AgentKind::Cursor
    {
        Ok(ChangeAction::Skip)
    } else if metadata.is_none() {
        Ok(ChangeAction::Add)
    } else if read_to_string_if_exists(destination)?.as_deref() == Some(content.as_str()) {
        Ok(ChangeAction::Unchanged)
    } else if target == AgentKind::Cursor {
        Ok(ChangeAction::Skip)
    } else {
        Ok(ChangeAction::Update)
    }
}

fn mcp_change_action(
    paths: &AgentPaths,
    mcp: &BTreeMap<String, McpServer>,
    name: &str,
    target: AgentKind,
) -> Result<ChangeAction> {
    let Some(server) = mcp.get(name) else {
        bail!("manifest MCP `{name}` is missing from mcp/servers.json");
    };
    let existing = match target {
        AgentKind::Codex => crate::mcp::discover_codex_mcp(&paths.codex_home.join("config.toml"))?,
        AgentKind::Claude => crate::mcp::discover_claude_mcp(&paths.claude_config)?,
        AgentKind::Cursor => {
            let configured_names = discover_cursor_mcp_names(&paths.cursor_config)?;
            let configured = discover_cursor_mcp(&paths.cursor_config)?;
            if let Some(existing) = configured.get(name) {
                return Ok(if existing == server {
                    ChangeAction::Unchanged
                } else {
                    ChangeAction::Skip
                });
            }
            if configured_names.contains(name) {
                return Ok(ChangeAction::Skip);
            }
            let project_owned = discover_cursor_project_mcp_names(&paths.cursor_home)?;
            if project_owned.contains(name) {
                return Ok(ChangeAction::Skip);
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

fn rendered_rule(pack: &Path, resource: &Resource, target: AgentKind) -> Result<String> {
    let raw = std::fs::read_to_string(pack.join(&resource.pack_path))?;
    match (resource.name.as_str(), target) {
        ("codex-agents", AgentKind::Claude) => Ok(format!(
            "# Imported Codex Agent Rules\n\nImported by `agent-sync` from pack resource `codex-agents`.\n\n{}",
            raw
        )),
        ("codex-agents", AgentKind::Cursor) => Ok(
            "---\ndescription: Bridge to Codex global agent rules\nalwaysApply: true\n---\n# Codex Agent Rule Bridge\n\nBefore starting a task, read and follow `~/.codex/AGENTS.md` when it exists. Treat it as shared guidance. Direct user instructions and Cursor-specific settings and rules take precedence if they conflict with that file.\n\nWhen prior work may matter, search the QMD `sessions` collection. QMD history is searchable context, not a resumable Cursor chat.\n"
                .to_string(),
        ),
        _ => Ok(raw),
    }
}

fn apply_mcp(
    paths: &AgentPaths,
    backup_root: &Path,
    mcp: &BTreeMap<String, McpServer>,
    target: AgentKind,
) -> Result<(Option<PathBuf>, Option<String>)> {
    match target {
        AgentKind::Claude => {
            let content = write_claude_mcp(&paths.claude_config, mcp)?;
            let installed_sha256 = hash_bytes(&content);
            let backup =
                replace_file_with_backup(backup_root, &paths.home, &paths.claude_config, &content)?;
            Ok((backup, Some(installed_sha256)))
        }
        AgentKind::Codex => {
            let path = paths.codex_home.join("config.toml");
            let content = write_codex_mcp(&path, mcp)?;
            let installed_sha256 = hash_bytes(&content);
            let backup = replace_file_with_backup(backup_root, &paths.codex_home, &path, &content)?;
            Ok((backup, Some(installed_sha256)))
        }
        AgentKind::Cursor => {
            for _ in 0..3 {
                let content = render_cursor_mcp_additions(paths, mcp)?;
                if paths.cursor_config.exists() && std::fs::read(&paths.cursor_config)? == content {
                    return Ok((None, None));
                }
                let latest = render_cursor_mcp_additions(paths, mcp)?;
                if latest != content {
                    continue;
                }
                let installed_sha256 = hash_bytes(&latest);
                let backup = replace_file_with_backup(
                    backup_root,
                    &paths.cursor_home,
                    &paths.cursor_config,
                    &latest,
                )?;
                return Ok((backup, Some(installed_sha256)));
            }
            bail!("Cursor MCP ownership changed repeatedly while applying; retry the sync")
        }
    }
}

fn render_cursor_mcp_additions(
    paths: &AgentPaths,
    mcp: &BTreeMap<String, McpServer>,
) -> Result<Vec<u8>> {
    let project_owned = discover_cursor_project_mcp_names(&paths.cursor_home)?;
    let addable = mcp
        .iter()
        .filter(|(name, _)| !project_owned.contains(*name))
        .map(|(name, server)| (name.clone(), server.clone()))
        .collect();
    write_cursor_mcp_additive(&paths.cursor_config, &addable)
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
            if let Err(error) = remove_path_if_present(&write.destination) {
                errors.push(format!("remove {}: {error:#}", write.destination.display()));
            }
            continue;
        };
        if let Err(error) = restore_backup_atomically(backup, &write.destination) {
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

fn remove_path_if_present(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("remove directory {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("remove file {}", path.display()))
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
) -> Result<()> {
    for resource in manifest
        .resources
        .iter()
        .filter(|resource| resource.kind == ResourceKind::Mcp && resource.targets.contains(&target))
    {
        let action = mcp_change_action(paths, mcp, &resource.name, target)?;
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
            && matches!(change.action, ChangeAction::Add | ChangeAction::Update)
    })
}

fn update_mcp_backup(changes: &mut [Change], target: AgentKind, backup: Option<PathBuf>) {
    for change in changes {
        if change.target == target
            && change.resource.starts_with("Mcp:")
            && matches!(change.action, ChangeAction::Add | ChangeAction::Update)
        {
            change.backup = backup.clone();
            break;
        }
    }
}

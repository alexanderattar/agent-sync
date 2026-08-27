use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    adapters::{AgentKind, AgentPaths},
    fsx::{
        ensure_dir, path_content_equal, read_to_string_if_exists, replace_dir_with_backup,
        replace_file_with_backup,
    },
    manifest::{Manifest, Resource, ResourceKind},
    mcp::{
        discover_cursor_effective_mcp_names, discover_cursor_mcp_names,
        discover_cursor_project_mcp_names, ensure_cursor_mcp_write_safe, load_pack_mcp,
        write_claude_mcp, write_codex_mcp, write_cursor_mcp_additive, McpServer,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApplyOptions {
    pub dry_run: bool,
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

    let backup_root = paths
        .home
        .join(".agent-sync")
        .join("backups")
        .join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    ensure_dir(&backup_root)?;

    let manifest = Manifest::load(pack)?;
    let mcp = load_pack_mcp(pack)?;
    let mut applied_mcp_targets = BTreeSet::new();
    for resource in &manifest.resources {
        for target in targets {
            if !resource.targets.contains(target) {
                continue;
            }
            if matches!(
                planned_action(&changes, *target, resource),
                Some(ChangeAction::Skip | ChangeAction::Unchanged)
            ) {
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
                    let root = agent_root(paths, *target);
                    let backup =
                        replace_dir_with_backup(&backup_root, root, &source, &destination)?;
                    update_backup(&mut changes, *target, resource, backup);
                }
                ResourceKind::Rule => {
                    let Some(destination) = destination_for(paths, resource, *target) else {
                        continue;
                    };
                    let content = rendered_rule(pack, resource, *target)?;
                    if read_to_string_if_exists(&destination)?.as_deref() == Some(content.as_str())
                    {
                        continue;
                    }
                    let root = agent_root(paths, *target);
                    let backup = replace_file_with_backup(
                        &backup_root,
                        root,
                        &destination,
                        content.as_bytes(),
                    )?;
                    update_backup(&mut changes, *target, resource, backup);
                }
                ResourceKind::Mcp => {
                    if !applied_mcp_targets.insert(*target) {
                        continue;
                    }
                    if !mcp_needs_apply(&changes, *target) {
                        continue;
                    }
                    let backup = apply_mcp(paths, &backup_root, &mcp, *target)?;
                    update_mcp_backup(&mut changes, *target, backup);
                }
            }
        }
    }

    Ok(ApplyReport {
        dry_run: false,
        changes,
        backup_root: Some(backup_root),
    })
}

pub fn verify_pack(paths: &AgentPaths, pack: &Path, targets: &[AgentKind]) -> Result<VerifyReport> {
    let manifest = Manifest::load(pack)?;
    let mcp = load_pack_mcp(pack)?;
    let mut checks = Vec::new();
    let mut errors = Vec::new();

    for resource in &manifest.resources {
        if resource.kind == ResourceKind::Mcp {
            continue;
        }
        for target in targets {
            if !resource.targets.contains(target) {
                continue;
            }
            let Some(destination) = destination_for(paths, resource, *target) else {
                continue;
            };
            if destination.exists() {
                checks.push(format!(
                    "{} {} installed at {}",
                    target,
                    resource_label(resource),
                    destination.display()
                ));
            } else {
                errors.push(format!(
                    "{} {} missing at {}",
                    target,
                    resource_label(resource),
                    destination.display()
                ));
            }
        }
    }

    for target in targets {
        match target {
            AgentKind::Claude => {
                let existing = crate::mcp::discover_claude_mcp(&paths.claude_config)?;
                for name in mcp.keys() {
                    if existing.contains_key(name) {
                        checks.push(format!("claude mcp `{name}` configured"));
                    } else {
                        errors.push(format!("claude mcp `{name}` missing"));
                    }
                }
            }
            AgentKind::Codex => {
                let existing =
                    crate::mcp::discover_codex_mcp(&paths.codex_home.join("config.toml"))?;
                for name in mcp.keys() {
                    if existing.contains_key(name) {
                        checks.push(format!("codex mcp `{name}` configured"));
                    } else {
                        errors.push(format!("codex mcp `{name}` missing"));
                    }
                }
            }
            AgentKind::Cursor => {
                let existing =
                    discover_cursor_effective_mcp_names(&paths.cursor_home, &paths.cursor_config)?;
                for name in mcp.keys() {
                    if existing.contains(name) {
                        checks.push(format!("cursor mcp `{name}` configured"));
                    } else {
                        errors.push(format!("cursor mcp `{name}` missing"));
                    }
                }
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
    if !destination.exists() {
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
    if !destination.exists() {
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
            let configured = discover_cursor_mcp_names(&paths.cursor_config)?;
            if configured.contains(name) {
                return Ok(ChangeAction::Unchanged);
            }
            let project_owned = discover_cursor_project_mcp_names(&paths.cursor_home)?;
            if project_owned.contains(name) {
                return Ok(ChangeAction::Skip);
            }
            ensure_cursor_mcp_write_safe(&paths.cursor_config)?;
            return Ok(ChangeAction::Add);
        }
    };
    Ok(if existing.get(name) == Some(server) {
        ChangeAction::Unchanged
    } else if existing.is_empty() {
        ChangeAction::Add
    } else {
        ChangeAction::Update
    })
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
) -> Result<Option<PathBuf>> {
    match target {
        AgentKind::Claude => {
            let content = write_claude_mcp(&paths.claude_config, mcp)?;
            replace_file_with_backup(backup_root, &paths.home, &paths.claude_config, &content)
        }
        AgentKind::Codex => {
            let path = paths.codex_home.join("config.toml");
            let content = write_codex_mcp(&path, mcp)?;
            replace_file_with_backup(backup_root, &paths.codex_home, &path, &content)
        }
        AgentKind::Cursor => {
            let project_owned = discover_cursor_project_mcp_names(&paths.cursor_home)?;
            let addable = mcp
                .iter()
                .filter(|(name, _)| !project_owned.contains(*name))
                .map(|(name, server)| (name.clone(), server.clone()))
                .collect();
            let content = write_cursor_mcp_additive(&paths.cursor_config, &addable)?;
            if paths.cursor_config.exists() && std::fs::read(&paths.cursor_config)? == content {
                return Ok(None);
            }
            replace_file_with_backup(
                backup_root,
                &paths.cursor_home,
                &paths.cursor_config,
                &content,
            )
        }
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

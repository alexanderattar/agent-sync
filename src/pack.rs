use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{bail, Context, Result};

use crate::{
    adapters::{AgentKind, AgentPaths},
    discover::{discover_agent, discover_shared_agents},
    fsx::{copy_dir_for_export, copy_file_for_export, ensure_dir, hash_path},
    manifest::{Manifest, Resource, ResourceKind},
    mcp::{
        discover_claude_mcp_for_export, discover_codex_mcp_for_export,
        discover_cursor_mcp_for_export, save_pack_mcp, McpServer,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceSelection {
    All,
    Codex,
    Claude,
    Cursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportOptions {
    pub source: SourceSelection,
    pub include_references: bool,
    pub include_mcp: bool,
    pub mcp_servers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportReport {
    pub pack: std::path::PathBuf,
    pub resources: usize,
    pub warnings: Vec<String>,
}

impl ExportReport {
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "Exported {} resources to {}\n",
            self.resources,
            self.pack.display()
        );
        for warning in &self.warnings {
            out.push_str(&format!("warning: {warning}\n"));
        }
        out
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitReport {
    pub pack: std::path::PathBuf,
    pub created_manifest: bool,
}

impl InitReport {
    pub fn to_text(&self) -> String {
        if self.created_manifest {
            format!("Initialized agent-sync pack at {}\n", self.pack.display())
        } else {
            format!(
                "Agent-sync pack already exists at {}\n",
                self.pack.display()
            )
        }
    }
}

pub fn init_pack(pack: &Path) -> Result<InitReport> {
    ensure_dir(pack)?;
    ensure_dir(&pack.join("skills"))?;
    ensure_dir(&pack.join("rules"))?;
    ensure_dir(&pack.join("mcp"))?;
    ensure_dir(&pack.join("references"))?;
    let manifest_path = pack.join(crate::manifest::MANIFEST_FILE);
    let created_manifest = !manifest_path.exists();
    if created_manifest {
        Manifest::new().save(pack)?;
    }
    Ok(InitReport {
        pack: pack.to_path_buf(),
        created_manifest,
    })
}

pub fn export_pack(
    paths: &AgentPaths,
    pack: &Path,
    options: ExportOptions,
) -> Result<ExportReport> {
    require_empty_export_pack(pack)?;
    ensure_dir(pack)?;
    let references = pack.join("references");
    ensure_dir(&pack.join("skills"))?;
    ensure_dir(&pack.join("rules"))?;
    ensure_dir(&pack.join("mcp"))?;
    ensure_dir(&references)?;

    let codex_inventory = if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Codex
    ) {
        Some(discover_agent(paths, AgentKind::Codex, false)?)
    } else {
        None
    };
    let claude_inventory = if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Claude
    ) {
        Some(discover_agent(paths, AgentKind::Claude, false)?)
    } else {
        None
    };
    let cursor_inventory = if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Cursor
    ) {
        Some(discover_agent(paths, AgentKind::Cursor, false)?)
    } else {
        None
    };
    let shared_inventory =
        if codex_inventory.is_some() || claude_inventory.is_some() || cursor_inventory.is_some() {
            Some(discover_shared_agents(paths)?)
        } else {
            None
        };
    let mut manifest = Manifest::new();
    let mut chosen_skills: BTreeMap<String, (String, std::path::PathBuf)> = BTreeMap::new();

    if let Some(inventory) = codex_inventory {
        choose_inventory_skills(
            &mut chosen_skills,
            &mut manifest.warnings,
            inventory,
            "codex",
        )?;
    }
    if let Some(inventory) = cursor_inventory {
        choose_inventory_skills(
            &mut chosen_skills,
            &mut manifest.warnings,
            inventory,
            "cursor",
        )?;
    }
    if let Some(inventory) = claude_inventory {
        choose_inventory_skills(
            &mut chosen_skills,
            &mut manifest.warnings,
            inventory,
            "claude",
        )?;
    }
    if let Some(inventory) = shared_inventory {
        choose_inventory_skills(
            &mut chosen_skills,
            &mut manifest.warnings,
            inventory,
            "agents",
        )?;
    }

    for (name, (source_agent, source_path)) in chosen_skills {
        let dest = pack.join("skills").join(&name);
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .with_context(|| format!("clear existing exported skill {}", dest.display()))?;
        }
        copy_dir_for_export(&source_path, &dest, &format!("skill `{name}`"))?;
        manifest.resources.push(Resource {
            kind: ResourceKind::Skill,
            name,
            source_agent,
            pack_path: dest.strip_prefix(pack)?.to_string_lossy().to_string(),
            sha256: hash_path(&dest)?,
            targets: vec![AgentKind::Codex, AgentKind::Claude, AgentKind::Cursor],
        });
    }

    export_rules(paths, pack, &options, &mut manifest)?;
    if options.include_references {
        export_references(paths, pack, &options, &mut manifest)?;
    }
    if options.include_mcp {
        export_mcp(paths, pack, &options, &mut manifest)?;
    }

    manifest.save(pack)?;
    Ok(ExportReport {
        pack: pack.to_path_buf(),
        resources: manifest.resources.len(),
        warnings: manifest.warnings,
    })
}

fn require_empty_export_pack(pack: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(pack) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", pack.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "export pack path must be a regular directory, not a symlink or file: {}",
            pack.display()
        );
    }

    for entry in fs::read_dir(pack).with_context(|| format!("read {}", pack.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect export pack entry {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("export pack contains a symlink: {}", path.display());
        }
        if matches!(
            name.to_str(),
            Some("skills" | "rules" | "mcp" | "references")
        ) {
            if !metadata.is_dir() || fs::read_dir(&path)?.next().transpose()?.is_some() {
                bail!(
                    "export requires an empty pack; {} already contains data",
                    path.display()
                );
            }
            continue;
        }
        if name == crate::manifest::MANIFEST_FILE {
            if !metadata.is_file() {
                bail!(
                    "export pack manifest is not a regular file: {}",
                    path.display()
                );
            }
            let manifest = Manifest::load(pack)?;
            if !manifest.resources.is_empty() || !manifest.warnings.is_empty() {
                bail!(
                    "export requires an empty pack; {} already describes resources",
                    path.display()
                );
            }
            continue;
        }
        bail!(
            "export requires an empty pack; unexpected entry {}",
            path.display()
        );
    }
    Ok(())
}

fn choose_inventory_skills(
    chosen: &mut BTreeMap<String, (String, std::path::PathBuf)>,
    warnings: &mut Vec<String>,
    inventory: crate::discover::AgentInventory,
    source_agent: &str,
) -> Result<()> {
    for skill in inventory.skills {
        match chosen.entry(skill.name.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((source_agent.to_string(), skill.path));
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if hash_path(&entry.get().1)? != hash_path(&skill.path)? {
                    warnings.push(format!(
                        "skill `{}` exists in multiple agents; kept {} copy",
                        skill.name,
                        entry.get().0
                    ));
                }
            }
        }
    }
    Ok(())
}

fn export_rules(
    paths: &AgentPaths,
    pack: &Path,
    options: &ExportOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Codex
    ) {
        let source = paths.codex_home.join("AGENTS.md");
        if source.exists() {
            let dest = pack.join("rules").join("codex-agents.md");
            copy_file_for_export(&source, &dest, "Codex agent rules")?;
            manifest.resources.push(Resource {
                kind: ResourceKind::Rule,
                name: "codex-agents".to_string(),
                source_agent: "codex".to_string(),
                pack_path: dest.strip_prefix(pack)?.to_string_lossy().to_string(),
                sha256: hash_path(&dest)?,
                targets: vec![AgentKind::Codex, AgentKind::Claude, AgentKind::Cursor],
            });
        }
    }

    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Claude
    ) {
        let source = paths.claude_home.join("CLAUDE.md");
        if source.exists() {
            let dest = pack.join("rules").join("claude-user.md");
            copy_file_for_export(&source, &dest, "Claude user rules")?;
            manifest.resources.push(Resource {
                kind: ResourceKind::Rule,
                name: "claude-user".to_string(),
                source_agent: "claude".to_string(),
                pack_path: dest.strip_prefix(pack)?.to_string_lossy().to_string(),
                sha256: hash_path(&dest)?,
                targets: vec![AgentKind::Claude],
            });
        }
    }
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Cursor
    ) {
        manifest.warnings.push(
            "Cursor rules stay Cursor-owned because their semantics do not map safely to Codex or Claude Code"
                .to_string(),
        );
    }
    Ok(())
}

fn export_references(
    paths: &AgentPaths,
    pack: &Path,
    options: &ExportOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Codex
    ) {
        let memories = paths.codex_home.join("memories");
        if memories.exists() {
            let dest = pack.join("references").join("codex-memories");
            if dest.exists() {
                fs::remove_dir_all(&dest)?;
            }
            copy_dir_for_export(&memories, &dest, "Codex memory references")?;
            manifest.resources.push(Resource {
                kind: ResourceKind::MemoryReference,
                name: "codex-memories".to_string(),
                source_agent: "codex".to_string(),
                pack_path: dest.strip_prefix(pack)?.to_string_lossy().to_string(),
                sha256: hash_path(&dest)?,
                targets: vec![AgentKind::Claude],
            });
        }

        let automations = paths.codex_home.join("automations");
        if automations.exists() {
            let dest = pack.join("references").join("codex-automations");
            if dest.exists() {
                fs::remove_dir_all(&dest)?;
            }
            copy_dir_for_export(&automations, &dest, "Codex automation references")?;
            manifest.resources.push(Resource {
                kind: ResourceKind::AutomationTemplate,
                name: "codex-automations".to_string(),
                source_agent: "codex".to_string(),
                pack_path: dest.strip_prefix(pack)?.to_string_lossy().to_string(),
                sha256: hash_path(&dest)?,
                targets: Vec::new(),
            });
        }
    }
    Ok(())
}

fn export_mcp(
    paths: &AgentPaths,
    pack: &Path,
    options: &ExportOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    let mut servers: BTreeMap<String, McpServer> = BTreeMap::new();
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Codex
    ) {
        for (name, server) in discover_codex_mcp_for_export(
            &paths.codex_home.join("config.toml"),
            &options.mcp_servers,
        )? {
            if !mcp_selected(options, &name) {
                continue;
            }
            servers.entry(name).or_insert(server);
        }
    }
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Claude
    ) {
        for (name, server) in
            discover_claude_mcp_for_export(&paths.claude_config, &options.mcp_servers)?
        {
            if !mcp_selected(options, &name) {
                continue;
            }
            servers.entry(name).or_insert(server);
        }
    }
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Cursor
    ) {
        for (name, server) in
            discover_cursor_mcp_for_export(&paths.cursor_config, &options.mcp_servers)?
        {
            servers.entry(name).or_insert(server);
        }
    }
    if servers.is_empty() {
        return Ok(());
    }
    save_pack_mcp(pack, &servers)?;
    let mcp_path = pack.join("mcp").join("servers.json");
    for name in servers.keys() {
        manifest.resources.push(Resource {
            kind: ResourceKind::Mcp,
            name: name.clone(),
            source_agent: "mixed".to_string(),
            pack_path: "mcp/servers.json".to_string(),
            sha256: hash_path(&mcp_path)?,
            targets: vec![AgentKind::Codex, AgentKind::Claude, AgentKind::Cursor],
        });
    }
    Ok(())
}

fn mcp_selected(options: &ExportOptions, name: &str) -> bool {
    options.mcp_servers.is_empty() || options.mcp_servers.iter().any(|selected| selected == name)
}

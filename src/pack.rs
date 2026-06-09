use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};

use crate::{
    adapters::{AgentKind, AgentPaths},
    discover::discover,
    fsx::{copy_dir, ensure_dir, hash_path},
    manifest::{Manifest, Resource, ResourceKind},
    mcp::{discover_claude_mcp, discover_codex_mcp, save_pack_mcp, McpServer},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceSelection {
    All,
    Codex,
    Claude,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportOptions {
    pub source: SourceSelection,
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
    ensure_dir(pack)?;
    ensure_dir(&pack.join("skills"))?;
    ensure_dir(&pack.join("rules"))?;
    ensure_dir(&pack.join("mcp"))?;
    ensure_dir(&pack.join("references"))?;

    let inventory = discover(paths)?;
    let mut manifest = Manifest::new();
    let mut chosen_skills: BTreeMap<String, (String, std::path::PathBuf)> = BTreeMap::new();

    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Codex
    ) {
        for skill in inventory.codex.skills {
            chosen_skills
                .entry(skill.name)
                .or_insert(("codex".to_string(), skill.path));
        }
        for skill in inventory.shared_agents.skills {
            chosen_skills
                .entry(skill.name)
                .or_insert(("agents".to_string(), skill.path));
        }
    }
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Claude
    ) {
        for skill in inventory.claude.skills {
            match chosen_skills.entry(skill.name.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(("claude".to_string(), skill.path));
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    if hash_path(&entry.get().1)? != hash_path(&skill.path)? {
                        manifest.warnings.push(format!(
                            "skill `{}` exists in multiple agents; kept {} copy",
                            skill.name,
                            entry.get().0
                        ));
                    }
                }
            }
        }
    }

    for (name, (source_agent, source_path)) in chosen_skills {
        let dest = pack.join("skills").join(&name);
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .with_context(|| format!("clear existing exported skill {}", dest.display()))?;
        }
        copy_dir(&source_path, &dest)?;
        manifest.resources.push(Resource {
            kind: ResourceKind::Skill,
            name,
            source_agent,
            pack_path: dest.strip_prefix(pack)?.to_string_lossy().to_string(),
            sha256: hash_path(&dest)?,
            targets: vec![AgentKind::Codex, AgentKind::Claude],
        });
    }

    export_rules(paths, pack, options, &mut manifest)?;
    export_references(paths, pack, options, &mut manifest)?;
    export_mcp(paths, pack, options, &mut manifest)?;

    manifest.save(pack)?;
    Ok(ExportReport {
        pack: pack.to_path_buf(),
        resources: manifest.resources.len(),
        warnings: manifest.warnings,
    })
}

fn export_rules(
    paths: &AgentPaths,
    pack: &Path,
    options: ExportOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Codex
    ) {
        let source = paths.codex_home.join("AGENTS.md");
        if source.exists() {
            let dest = pack.join("rules").join("codex-agents.md");
            fs::copy(&source, &dest)
                .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
            manifest.resources.push(Resource {
                kind: ResourceKind::Rule,
                name: "codex-agents".to_string(),
                source_agent: "codex".to_string(),
                pack_path: dest.strip_prefix(pack)?.to_string_lossy().to_string(),
                sha256: hash_path(&dest)?,
                targets: vec![AgentKind::Codex, AgentKind::Claude],
            });
        }
    }

    if matches!(options.source, SourceSelection::Claude) {
        let source = paths.claude_home.join("CLAUDE.md");
        if source.exists() {
            let dest = pack.join("rules").join("claude-user.md");
            fs::copy(&source, &dest)?;
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
    Ok(())
}

fn export_references(
    paths: &AgentPaths,
    pack: &Path,
    options: ExportOptions,
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
            copy_dir(&memories, &dest)?;
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
            copy_dir(&automations, &dest)?;
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
    options: ExportOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    let mut servers: BTreeMap<String, McpServer> = BTreeMap::new();
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Codex
    ) {
        for (name, server) in discover_codex_mcp(&paths.codex_home.join("config.toml"))? {
            servers.entry(name).or_insert(server);
        }
    }
    if matches!(
        options.source,
        SourceSelection::All | SourceSelection::Claude
    ) {
        for (name, server) in discover_claude_mcp(&paths.claude_config)? {
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
            targets: vec![AgentKind::Codex, AgentKind::Claude],
        });
    }
    Ok(())
}

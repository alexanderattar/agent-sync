use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    adapters::AgentPaths,
    fsx::{list_named_skill_dirs, read_to_string_if_exists},
    mcp::{discover_claude_mcp, discover_codex_mcp},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Inventory {
    pub codex: AgentInventory,
    pub claude: AgentInventory,
    pub shared_agents: AgentInventory,
}

impl Inventory {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.codex.push_text("Codex", &mut out);
        self.claude.push_text("Claude", &mut out);
        self.shared_agents.push_text("Shared .agents", &mut out);
        out
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AgentInventory {
    pub home: PathBuf,
    pub skills: Vec<NamedPath>,
    pub rules: Vec<NamedPath>,
    pub mcp_servers: Vec<String>,
    pub memories: Vec<NamedPath>,
    pub automations: Vec<NamedPath>,
}

impl AgentInventory {
    fn push_text(&self, heading: &str, out: &mut String) {
        out.push_str(&format!("{heading} ({})\n", self.home.display()));
        out.push_str(&format!("  skills: {}\n", self.skills.len()));
        for item in &self.skills {
            out.push_str(&format!("    - {} ({})\n", item.name, item.path.display()));
        }
        out.push_str(&format!("  rules: {}\n", self.rules.len()));
        for item in &self.rules {
            out.push_str(&format!("    - {} ({})\n", item.name, item.path.display()));
        }
        out.push_str(&format!("  mcp servers: {}\n", self.mcp_servers.len()));
        for item in &self.mcp_servers {
            out.push_str(&format!("    - {item}\n"));
        }
        out.push_str(&format!("  memories: {}\n", self.memories.len()));
        out.push_str(&format!("  automations: {}\n", self.automations.len()));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NamedPath {
    pub name: String,
    pub path: PathBuf,
}

pub fn discover(paths: &AgentPaths) -> Result<Inventory> {
    Ok(Inventory {
        codex: discover_codex(paths)?,
        claude: discover_claude(paths)?,
        shared_agents: discover_agents(paths)?,
    })
}

fn discover_codex(paths: &AgentPaths) -> Result<AgentInventory> {
    let home = paths.codex_home.clone();
    let skills = list_named_skill_dirs(&home.join("skills"))?
        .into_iter()
        .map(|(name, path)| NamedPath { name, path })
        .collect();

    let mut rules = Vec::new();
    if home.join("AGENTS.md").exists() {
        rules.push(NamedPath {
            name: "AGENTS.md".to_string(),
            path: home.join("AGENTS.md"),
        });
    }

    let memories = list_files_named(&home.join("memories"), &["MEMORY.md", "memory_summary.md"])?;
    let automations = list_automation_files(&home.join("automations"))?;
    let mcp_servers = discover_codex_mcp(&home.join("config.toml"))?
        .into_keys()
        .collect();

    Ok(AgentInventory {
        home,
        skills,
        rules,
        mcp_servers,
        memories,
        automations,
    })
}

fn discover_claude(paths: &AgentPaths) -> Result<AgentInventory> {
    let home = paths.claude_home.clone();
    let skills = list_named_skill_dirs(&home.join("skills"))?
        .into_iter()
        .map(|(name, path)| NamedPath { name, path })
        .collect();

    let mut rules = Vec::new();
    if home.join("CLAUDE.md").exists() {
        rules.push(NamedPath {
            name: "CLAUDE.md".to_string(),
            path: home.join("CLAUDE.md"),
        });
    }
    let rules_dir = home.join("rules");
    if rules_dir.exists() {
        for entry in
            fs::read_dir(&rules_dir).with_context(|| format!("read {}", rules_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                rules.push(NamedPath {
                    name: entry.file_name().to_string_lossy().to_string(),
                    path,
                });
            }
        }
    }

    let mcp_servers = discover_claude_mcp(&paths.claude_config)?
        .into_keys()
        .collect();

    Ok(AgentInventory {
        home,
        skills,
        rules,
        mcp_servers,
        memories: Vec::new(),
        automations: Vec::new(),
    })
}

fn discover_agents(paths: &AgentPaths) -> Result<AgentInventory> {
    let home = paths.agents_home.clone();
    let skills = list_named_skill_dirs(&home.join("skills"))?
        .into_iter()
        .map(|(name, path)| NamedPath { name, path })
        .collect();
    Ok(AgentInventory {
        home,
        skills,
        ..AgentInventory::default()
    })
}

fn list_files_named(root: &std::path::Path, names: &[&str]) -> Result<Vec<NamedPath>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for name in names {
        let path = root.join(name);
        if read_to_string_if_exists(&path)?.is_some() {
            out.push(NamedPath {
                name: (*name).to_string(),
                path,
            });
        }
    }
    Ok(out)
}

fn list_automation_files(root: &std::path::Path) -> Result<Vec<NamedPath>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.join("automation.toml").exists() {
            out.push(NamedPath {
                name: entry.file_name().to_string_lossy().to_string(),
                path: path.join("automation.toml"),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

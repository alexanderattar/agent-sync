use std::{
    env,
    fmt::{Display, Formatter},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Codex,
    Claude,
    Cursor,
    Amp,
}

impl Display for AgentKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentKind::Codex => f.write_str("codex"),
            AgentKind::Claude => f.write_str("claude"),
            AgentKind::Cursor => f.write_str("cursor"),
            AgentKind::Amp => f.write_str("amp"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPaths {
    pub home: PathBuf,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub claude_config: PathBuf,
    pub cursor_home: PathBuf,
    pub cursor_config: PathBuf,
    pub agents_home: PathBuf,
    pub amp_home: PathBuf,
    pub amp_config: PathBuf,
}

impl AgentPaths {
    pub fn from_optional(
        home: Option<PathBuf>,
        codex_home: Option<PathBuf>,
        claude_home: Option<PathBuf>,
        claude_config: Option<PathBuf>,
        cursor_home: Option<PathBuf>,
        cursor_config: Option<PathBuf>,
        agents_home: Option<PathBuf>,
    ) -> Result<Self> {
        let home = home.map_or_else(home_dir, Ok)?;
        let codex_home = codex_home.unwrap_or_else(|| home.join(".codex"));
        let claude_home = claude_home.unwrap_or_else(|| home.join(".claude"));
        let claude_config =
            claude_config.unwrap_or_else(|| default_claude_config(&home, &claude_home));
        let cursor_home = cursor_home.unwrap_or_else(|| home.join(".cursor"));
        let cursor_config = cursor_config.unwrap_or_else(|| cursor_home.join("mcp.json"));
        let agents_home = agents_home.unwrap_or_else(|| home.join(".agents"));
        let amp_home = home.join(".config/amp");
        let amp_config = amp_home.join("settings.json");

        Ok(Self {
            home,
            codex_home,
            claude_home,
            claude_config,
            cursor_home,
            cursor_config,
            agents_home,
            amp_home,
            amp_config,
        })
    }

    pub fn for_test(root: &Path) -> Self {
        Self {
            home: root.to_path_buf(),
            codex_home: root.join(".codex"),
            claude_home: root.join(".claude"),
            claude_config: root.join(".claude.json"),
            cursor_home: root.join(".cursor"),
            cursor_config: root.join(".cursor/mcp.json"),
            agents_home: root.join(".agents"),
            amp_home: root.join(".config/amp"),
            amp_config: root.join(".config/amp/settings.json"),
        }
    }

    /// Personal skill locations in Amp's documented precedence order.
    pub fn amp_skill_dirs(&self) -> Result<Vec<PathBuf>> {
        let settings: serde_json::Value = match std::fs::read(&self.amp_config) {
            Ok(raw) => serde_json::from_slice(&raw)
                .with_context(|| format!("parse {}", self.amp_config.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.amp_config.display()))
            }
        };
        let mut directories = Vec::new();
        if settings
            .get("amp.skills.disableGlobalAgentsSkills")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            directories.push(self.home.join(".config/agents/skills"));
            directories.push(self.agents_home.join("skills"));
        }
        directories.push(self.amp_home.join("skills"));
        if settings
            .get("amp.skills.disableClaudeCodeSkills")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            directories.push(self.claude_home.join("skills"));
        }
        Ok(directories)
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME").map(PathBuf::from).context(
        "HOME is not set; pass explicit --codex-home, --claude-home, and --cursor-home paths",
    )
}

fn default_claude_config(home: &Path, claude_home: &Path) -> PathBuf {
    if claude_home.file_name().and_then(|name| name.to_str()) == Some(".claude") {
        claude_home
            .parent()
            .map(|parent| parent.join(".claude.json"))
            .unwrap_or_else(|| home.join(".claude.json"))
    } else {
        home.join(".claude.json")
    }
}

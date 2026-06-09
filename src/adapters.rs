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
}

impl Display for AgentKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentKind::Codex => f.write_str("codex"),
            AgentKind::Claude => f.write_str("claude"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPaths {
    pub home: PathBuf,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub claude_config: PathBuf,
    pub agents_home: PathBuf,
}

impl AgentPaths {
    pub fn from_optional(
        home: Option<PathBuf>,
        codex_home: Option<PathBuf>,
        claude_home: Option<PathBuf>,
        claude_config: Option<PathBuf>,
        agents_home: Option<PathBuf>,
    ) -> Result<Self> {
        let home = home.map_or_else(home_dir, Ok)?;
        let codex_home = codex_home.unwrap_or_else(|| home.join(".codex"));
        let claude_home = claude_home.unwrap_or_else(|| home.join(".claude"));
        let claude_config =
            claude_config.unwrap_or_else(|| default_claude_config(&home, &claude_home));
        let agents_home = agents_home.unwrap_or_else(|| home.join(".agents"));

        Ok(Self {
            home,
            codex_home,
            claude_home,
            claude_config,
            agents_home,
        })
    }

    pub fn for_test(root: &Path) -> Self {
        Self {
            home: root.to_path_buf(),
            codex_home: root.join(".codex"),
            claude_home: root.join(".claude"),
            claude_config: root.join(".claude.json"),
            agents_home: root.join(".agents"),
        }
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; pass explicit --codex-home and --claude-home paths")
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

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{adapters::AgentKind, fsx::write_atomic};

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_STALE_AFTER_HOURS: u64 = 36;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub source: CanonicalSource,
    pub targets: Vec<AgentKind>,
    #[serde(default)]
    pub include_references: bool,
    #[serde(default)]
    pub allow_updates: bool,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub cursor_history: CursorHistoryConfig,
    #[serde(default)]
    pub health: HealthConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            source: CanonicalSource::Codex,
            targets: vec![AgentKind::Cursor],
            include_references: false,
            allow_updates: false,
            mcp: McpConfig::default(),
            cursor_history: CursorHistoryConfig::default(),
            health: HealthConfig::default(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported config version {}; expected {CONFIG_VERSION}",
                self.version
            );
        }
        if self.targets.is_empty() {
            bail!("at least one target is required");
        }

        let source = self.source.agent_kind();
        let mut targets = BTreeSet::new();
        for target in &self.targets {
            if *target == source {
                bail!("source {source} cannot also be a target");
            }
            if !targets.insert(*target) {
                bail!("target {target} is listed more than once");
            }
        }

        self.mcp.validate()?;
        if self.cursor_history.enabled
            && self.source != CanonicalSource::Cursor
            && !self.targets.contains(&AgentKind::Cursor)
        {
            bail!("cursor_history.enabled requires cursor as the source or a target");
        }
        if self.cursor_history.refresh_qmd && !self.cursor_history.enabled {
            bail!("cursor_history.refresh_qmd requires cursor_history.enabled");
        }
        if self.health.stale_after_hours == 0 {
            bail!("health.stale_after_hours must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CanonicalSource {
    Codex,
    Claude,
    Cursor,
}

impl CanonicalSource {
    pub const fn agent_kind(self) -> AgentKind {
        match self {
            Self::Codex => AgentKind::Codex,
            Self::Claude => AgentKind::Claude,
            Self::Cursor => AgentKind::Cursor,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub mode: McpMode,
    #[serde(default)]
    pub servers: Vec<String>,
}

impl McpConfig {
    fn validate(&self) -> Result<()> {
        match self.mode {
            McpMode::Selected if self.servers.is_empty() => {
                bail!("mcp mode selected requires at least one server");
            }
            McpMode::None | McpMode::All if !self.servers.is_empty() => {
                bail!("mcp server names are only valid in selected mode");
            }
            _ => {}
        }

        let mut names = BTreeSet::new();
        for name in &self.servers {
            if name.is_empty() || name.trim() != name {
                bail!("MCP server names must be non-empty and have no surrounding whitespace");
            }
            if !names.insert(name) {
                bail!("MCP server `{name}` is listed more than once");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMode {
    #[default]
    None,
    Selected,
    All,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CursorHistoryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub refresh_qmd: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    #[serde(default = "default_stale_after_hours")]
    pub stale_after_hours: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            stale_after_hours: DEFAULT_STALE_AFTER_HOURS,
        }
    }
}

pub fn default_config_path(home: &Path) -> PathBuf {
    home.join(".agent-sync").join("config.toml")
}

pub fn resolve_config_path(home: &Path, explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_config_path(home))
}

pub fn parse_config(raw: &str) -> Result<Config> {
    let config: Config = toml_edit::de::from_str(raw).context("parse agent-sync config")?;
    config.validate()?;
    Ok(config)
}

pub fn load_config(path: &Path) -> Result<Config> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_config(&raw).with_context(|| format!("load {}", path.display()))
}

pub fn render_config(config: &Config) -> Result<String> {
    config.validate()?;
    let mut rendered =
        toml_edit::ser::to_string_pretty(config).context("render agent-sync config")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

pub fn save_config(path: &Path, config: &Config) -> Result<()> {
    let rendered = render_config(config)?;
    write_atomic(path, rendered.as_bytes())
        .with_context(|| format!("save agent-sync config {}", path.display()))
}

const fn default_stale_after_hours() -> u64 {
    DEFAULT_STALE_AFTER_HOURS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_safe_defaults() {
        let config = parse_config(
            r#"
version = 1
source = "codex"
targets = ["cursor"]
"#,
        )
        .unwrap();

        assert!(!config.include_references);
        assert!(!config.allow_updates);
        assert_eq!(config.mcp.mode, McpMode::None);
        assert!(config.mcp.servers.is_empty());
        assert!(!config.cursor_history.enabled);
        assert!(!config.cursor_history.refresh_qmd);
        assert_eq!(config.health.stale_after_hours, 36);
    }

    #[test]
    fn validation_rejects_loops_and_ambiguous_mcp_selection() {
        let mut config = Config {
            targets: vec![AgentKind::Codex],
            ..Config::default()
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("source"));

        config.targets = vec![AgentKind::Cursor];
        config.mcp.mode = McpMode::Selected;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("at least one server"));

        config.mcp.mode = McpMode::All;
        config.mcp.servers = vec!["qmd".to_string()];
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("only valid in selected mode"));
    }

    #[test]
    fn path_resolution_honors_an_explicit_override() {
        let home = Path::new("/tmp/example-home");
        assert_eq!(
            resolve_config_path(home, None),
            home.join(".agent-sync/config.toml")
        );
        assert_eq!(
            resolve_config_path(home, Some(Path::new("/tmp/custom.toml"))),
            PathBuf::from("/tmp/custom.toml")
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let path = default_config_path(temp.path());
        let mut expected = Config::default();
        expected.mcp.mode = McpMode::Selected;
        expected.mcp.servers = vec!["qmd".to_string(), "exa".to_string()];
        expected.cursor_history.enabled = true;

        save_config(&path, &expected).unwrap();

        assert_eq!(load_config(&path).unwrap(), expected);
        assert!(fs::read_to_string(path).unwrap().ends_with('\n'));
    }
}

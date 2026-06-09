use std::{fs, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::adapters::AgentKind;

pub const MANIFEST_FILE: &str = "agent-sync.manifest.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub resources: Vec<Resource>,
    pub warnings: Vec<String>,
}

impl Manifest {
    pub fn new() -> Self {
        Self {
            version: 1,
            generated_at: Utc::now(),
            resources: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn load(pack: &Path) -> Result<Self> {
        let path = pack.join(MANIFEST_FILE);
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, pack: &Path) -> Result<()> {
        let path = pack.join(MANIFEST_FILE);
        let raw = serde_json::to_vec_pretty(self)?;
        crate::fsx::write_atomic(&path, &[raw, b"\n".to_vec()].concat())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Resource {
    pub kind: ResourceKind,
    pub name: String,
    pub source_agent: String,
    pub pack_path: String,
    pub sha256: String,
    pub targets: Vec<AgentKind>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceKind {
    Skill,
    Rule,
    Mcp,
    MemoryReference,
    AutomationTemplate,
}

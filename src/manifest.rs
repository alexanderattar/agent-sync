use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    path::{Component, Path},
};

use anyhow::{bail, Context, Result};
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
        let manifest: Self =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        manifest.validate(pack)?;
        Ok(manifest)
    }

    pub fn save(&self, pack: &Path) -> Result<()> {
        let path = pack.join(MANIFEST_FILE);
        let raw = serde_json::to_vec_pretty(self)?;
        crate::fsx::write_atomic(&path, &[raw, b"\n".to_vec()].concat())
    }

    fn validate(&self, pack: &Path) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported manifest version {}", self.version);
        }

        let mut hashes = BTreeMap::<&str, &str>::new();
        let mut identities = BTreeSet::new();
        for resource in &self.resources {
            if matches!(
                resource.kind,
                ResourceKind::Skill | ResourceKind::MemoryReference
            ) {
                let mut name_components = Path::new(&resource.name).components();
                let safe_name = matches!(
                    (name_components.next(), name_components.next()),
                    (Some(Component::Normal(component)), None)
                        if component == OsStr::new(&resource.name)
                );
                if !safe_name {
                    bail!(
                        "manifest {:?} resource has unsafe name `{}`; names must be one path component",
                        resource.kind,
                        resource.name
                    );
                }
            }
            for target in &resource.targets {
                if !identities.insert((resource.kind, resource.name.as_str(), *target)) {
                    bail!(
                        "manifest contains duplicate {:?} resource `{}` for {}",
                        resource.kind,
                        resource.name,
                        target
                    );
                }
            }
            let relative = Path::new(&resource.pack_path);
            if resource.pack_path.is_empty()
                || relative.is_absolute()
                || relative.components().any(|part| {
                    matches!(
                        part,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                bail!(
                    "manifest resource `{}` has unsafe pack path `{}`",
                    resource.name,
                    resource.pack_path
                );
            }
            if let Some(existing) = hashes.insert(&resource.pack_path, &resource.sha256) {
                if existing != resource.sha256 {
                    bail!(
                        "manifest path `{}` has conflicting hashes",
                        resource.pack_path
                    );
                }
                continue;
            }
            let source = pack.join(relative);
            if !source.exists() {
                bail!(
                    "manifest resource `{}` is missing at {}",
                    resource.name,
                    source.display()
                );
            }
            let actual = crate::fsx::hash_path(&source)?;
            if actual != resource.sha256 {
                bail!(
                    "manifest resource `{}` failed hash validation at {}",
                    resource.name,
                    source.display()
                );
            }
        }
        Ok(())
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceKind {
    Skill,
    Rule,
    Mcp,
    MemoryReference,
    AutomationTemplate,
}

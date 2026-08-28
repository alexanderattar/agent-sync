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
        self.validate(pack)?;
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
        let mut mcp_resource_names = BTreeSet::new();
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
            if resource.kind == ResourceKind::Mcp {
                if resource.name.is_empty() {
                    bail!("manifest MCP resource name must not be empty");
                }
                if resource.pack_path != "mcp/servers.json" {
                    bail!(
                        "manifest MCP resource `{}` must use pack path `mcp/servers.json`, found `{}`",
                        resource.name,
                        resource.pack_path
                    );
                }
                if !mcp_resource_names.insert(resource.name.clone()) {
                    bail!(
                        "manifest contains duplicate MCP resource `{}`",
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
        validate_mcp_authorization(pack, &mcp_resource_names)?;
        Ok(())
    }
}

fn validate_mcp_authorization(pack: &Path, manifest_names: &BTreeSet<String>) -> Result<()> {
    let path = pack.join("mcp").join("servers.json");
    let server_names = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("pack MCP server map is a symlink: {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!(
                "pack MCP server map is not a regular file: {}",
                path.display()
            )
        }
        Ok(_) => {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("read MCP server map {}", path.display()))?;
            let value: serde_json::Value = serde_json::from_str(&raw)
                .with_context(|| format!("parse MCP server map {}", path.display()))?;
            value
                .as_object()
                .with_context(|| {
                    format!("MCP server map {} must be a JSON object", path.display())
                })?
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect MCP server map {}", path.display()))
        }
    };

    let unlisted = server_names
        .difference(manifest_names)
        .cloned()
        .collect::<Vec<_>>();
    let missing = manifest_names
        .difference(&server_names)
        .cloned()
        .collect::<Vec<_>>();
    if !unlisted.is_empty() || !missing.is_empty() {
        let mut details = Vec::new();
        if !unlisted.is_empty() {
            details.push(format!("unlisted server(s): {}", unlisted.join(", ")));
        }
        if !missing.is_empty() {
            details.push(format!(
                "manifest server(s) missing from map: {}",
                missing.join(", ")
            ));
        }
        bail!("MCP authorization mismatch: {}", details.join("; "));
    }
    Ok(())
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

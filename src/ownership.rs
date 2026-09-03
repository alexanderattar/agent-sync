use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    adapters::AgentPaths,
    manifest::{Resource, ResourceKind},
};

const CLAUDE_OWNERSHIP_STATE_SCHEMA_VERSION: u32 = 1;
const CLAUDE_OWNERSHIP_STATE_FILE: &str = "claude-resources.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClaudeOwnershipState {
    schema_version: u32,
    claude_root: PathBuf,
    resources: BTreeMap<String, ClaudeOwnedResource>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClaudeOwnedResource {
    destination: PathBuf,
    installed_sha256: String,
}

/// A validated snapshot of the Claude resources that agent-sync installed.
///
/// Callers must record a resource only after agent-sync successfully writes it.
/// Matching target content without a record remains target-owned.
#[derive(Debug)]
pub(crate) struct ClaudeResourceOwnership {
    path: PathBuf,
    original: Option<Vec<u8>>,
    state: ClaudeOwnershipState,
}

impl ClaudeResourceOwnership {
    pub(crate) fn load(paths: &AgentPaths) -> Result<Self> {
        let path = claude_ownership_state_path(paths);
        ensure_safe_state_path(&path)?;
        let original = read_optional_bytes(&path)?;
        let state = match original.as_deref() {
            Some(raw) => serde_json::from_slice(raw).with_context(|| {
                format!("parse Claude resource ownership state {}", path.display())
            })?,
            None => ClaudeOwnershipState {
                schema_version: CLAUDE_OWNERSHIP_STATE_SCHEMA_VERSION,
                claude_root: paths.claude_home.clone(),
                resources: BTreeMap::new(),
            },
        };
        validate_state(&state, &paths.claude_home, &path)?;
        Ok(Self {
            path,
            original,
            state,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn raw(&self) -> Option<&[u8]> {
        self.original.as_deref()
    }

    /// Returns true only when state records this exact destination and its
    /// current content still matches the content installed by agent-sync.
    pub(crate) fn is_unmodified(
        &self,
        resource: &Resource,
        destination: &Path,
        current_sha256: &str,
    ) -> Result<bool> {
        require_sha256(current_sha256, "current Claude resource")?;
        let identity = resource_identity(resource)?;
        let Some(owned) = self.state.resources.get(&identity) else {
            return Ok(false);
        };
        if owned.destination != destination {
            return Ok(false);
        }
        Ok(owned.installed_sha256 == current_sha256)
    }

    /// Records content that agent-sync just installed at a Claude destination.
    pub(crate) fn record(
        &mut self,
        resource: &Resource,
        destination: PathBuf,
        installed_sha256: String,
    ) -> Result<()> {
        require_sha256(&installed_sha256, "installed Claude resource")?;
        validate_destination(&self.state.claude_root, &destination, &self.path)?;
        let identity = resource_identity(resource)?;
        self.state.resources.insert(
            identity,
            ClaudeOwnedResource {
                destination,
                installed_sha256,
            },
        );
        Ok(())
    }

    pub(crate) fn render(&self) -> Result<Vec<u8>> {
        validate_state(&self.state, &self.state.claude_root, &self.path)?;
        Ok([serde_json::to_vec_pretty(&self.state)?, b"\n".to_vec()].concat())
    }
}

pub(crate) fn claude_ownership_state_path(paths: &AgentPaths) -> PathBuf {
    paths
        .home
        .join(".agent-sync")
        .join("state")
        .join(CLAUDE_OWNERSHIP_STATE_FILE)
}

fn validate_state(state: &ClaudeOwnershipState, claude_root: &Path, path: &Path) -> Result<()> {
    if state.schema_version != CLAUDE_OWNERSHIP_STATE_SCHEMA_VERSION {
        bail!(
            "unsupported Claude resource ownership state version {} in {}",
            state.schema_version,
            path.display()
        );
    }
    if state.claude_root != claude_root {
        bail!(
            "refusing to use Claude resource ownership state for {}; it records {}",
            claude_root.display(),
            state.claude_root.display()
        );
    }
    for (identity, owned) in &state.resources {
        validate_identity(identity, path)?;
        validate_destination(claude_root, &owned.destination, path)?;
        require_sha256(
            &owned.installed_sha256,
            &format!("Claude resource `{identity}`"),
        )?;
    }
    Ok(())
}

fn resource_identity(resource: &Resource) -> Result<String> {
    let kind = match resource.kind {
        ResourceKind::Skill => "skill",
        ResourceKind::Rule => "rule",
        other => bail!("Claude ownership does not support {other:?} resources"),
    };
    require_safe_name(&resource.name)?;
    Ok(format!("{kind}:{}", resource.name))
}

fn validate_identity(identity: &str, path: &Path) -> Result<()> {
    let Some((kind, name)) = identity.split_once(':') else {
        bail!(
            "Claude resource ownership state has invalid identity `{identity}` in {}",
            path.display()
        );
    };
    if !matches!(kind, "skill" | "rule") || require_safe_name(name).is_err() {
        bail!(
            "Claude resource ownership state has invalid identity `{identity}` in {}",
            path.display()
        );
    }
    Ok(())
}

fn require_safe_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    let safe = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == OsStr::new(name)
    );
    if !safe {
        bail!("Claude resource ownership name `{name}` must be one path component");
    }
    Ok(())
}

fn validate_destination(claude_root: &Path, destination: &Path, path: &Path) -> Result<()> {
    let relative = destination.strip_prefix(claude_root).with_context(|| {
        format!(
            "Claude resource ownership destination {} is outside {} in {}",
            destination.display(),
            claude_root.display(),
            path.display()
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!(
            "Claude resource ownership destination {} is invalid in {}",
            destination.display(),
            path.display()
        );
    }
    Ok(())
}

fn require_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} has an invalid SHA-256 hash");
    }
    Ok(())
}

fn ensure_safe_state_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to read or replace symlinked Claude resource ownership state {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "Claude resource ownership state path {} is not a regular file",
            path.display()
        );
    }
    Ok(())
}

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AgentKind;

    fn resource(kind: ResourceKind, name: &str) -> Resource {
        Resource {
            kind,
            name: name.to_string(),
            source_agent: "codex".to_string(),
            pack_path: String::new(),
            sha256: String::new(),
            targets: vec![AgentKind::Claude],
        }
    }

    #[test]
    fn only_an_explicit_install_record_establishes_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let skill = resource(ResourceKind::Skill, "review");
        let destination = paths.claude_home.join("skills/review");
        let installed = "a".repeat(64);
        let mut snapshot = ClaudeResourceOwnership::load(&paths).unwrap();

        assert!(!snapshot
            .is_unmodified(&skill, &destination, &installed)
            .unwrap());

        snapshot
            .record(&skill, destination.clone(), installed.clone())
            .unwrap();

        assert!(snapshot
            .is_unmodified(&skill, &destination, &installed)
            .unwrap());
        assert!(!snapshot
            .is_unmodified(&skill, &destination, &"b".repeat(64))
            .unwrap());
    }

    #[test]
    fn a_changed_destination_is_unowned_until_agent_sync_installs_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let rule = resource(ResourceKind::Rule, "codex-agents");
        let imported = paths.claude_home.join("rules/imported-codex-agents.md");
        let legacy = paths.claude_home.join("rules/codex-global-agent-rules.md");
        let mut snapshot = ClaudeResourceOwnership::load(&paths).unwrap();

        snapshot.record(&rule, imported, "a".repeat(64)).unwrap();
        assert!(!snapshot
            .is_unmodified(&rule, &legacy, &"a".repeat(64))
            .unwrap());

        snapshot
            .record(&rule, legacy.clone(), "b".repeat(64))
            .unwrap();
        assert!(snapshot
            .is_unmodified(&rule, &legacy, &"b".repeat(64))
            .unwrap());
    }

    #[test]
    fn rendering_is_deterministic_and_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let mut snapshot = ClaudeResourceOwnership::load(&paths).unwrap();
        let rule = resource(ResourceKind::Rule, "codex-agents");
        let skill = resource(ResourceKind::Skill, "alpha");
        snapshot
            .record(
                &rule,
                paths.claude_home.join("rules/imported-codex-agents.md"),
                "c".repeat(64),
            )
            .unwrap();
        snapshot
            .record(
                &skill,
                paths.claude_home.join("skills/alpha"),
                "d".repeat(64),
            )
            .unwrap();
        let rendered = snapshot.render().unwrap();
        let text = String::from_utf8(rendered.clone()).unwrap();
        assert!(text.find("rule:codex-agents").unwrap() < text.find("skill:alpha").unwrap());

        fs::create_dir_all(snapshot.path().parent().unwrap()).unwrap();
        fs::write(snapshot.path(), rendered).unwrap();
        let loaded = ClaudeResourceOwnership::load(&paths).unwrap();

        assert!(loaded
            .is_unmodified(
                &rule,
                &paths.claude_home.join("rules/imported-codex-agents.md"),
                &"c".repeat(64),
            )
            .unwrap());
        assert!(loaded
            .is_unmodified(
                &skill,
                &paths.claude_home.join("skills/alpha"),
                &"d".repeat(64),
            )
            .unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let state_path = claude_ownership_state_path(&paths);
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let target = temp.path().join("target.json");
        fs::write(&target, "{}\n").unwrap();
        symlink(target, state_path).unwrap();

        let error = ClaudeResourceOwnership::load(&paths).unwrap_err();

        assert!(error.to_string().contains("symlinked Claude resource"));
    }
}

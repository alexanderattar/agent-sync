use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    adapters::{AgentKind, AgentPaths},
    manifest::{Resource, ResourceKind},
};

const RESOURCE_OWNERSHIP_STATE_SCHEMA_VERSION: u32 = 1;
const CLAUDE_OWNERSHIP_STATE_FILE: &str = "claude-resources.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ResourceOwnershipState {
    schema_version: u32,
    #[serde(alias = "claude_root")]
    root: PathBuf,
    resources: BTreeMap<String, OwnedResource>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct OwnedResource {
    destination: PathBuf,
    installed_sha256: String,
}

/// A validated snapshot of the Agent resources that agent-sync installed.
///
/// Callers must record a resource only after agent-sync successfully writes it.
/// Matching target content without a record remains target-owned.
#[derive(Debug)]
pub(crate) struct ResourceOwnership {
    path: PathBuf,
    original: Option<Vec<u8>>,
    state: ResourceOwnershipState,
    agent: AgentKind,
}

#[cfg(test)]
type ClaudeResourceOwnership = ResourceOwnership;

impl ResourceOwnership {
    #[cfg(test)]
    pub(crate) fn load(paths: &AgentPaths) -> Result<Self> {
        Self::load_for(paths, AgentKind::Claude)
    }

    pub(crate) fn load_for(paths: &AgentPaths, agent: AgentKind) -> Result<Self> {
        let (path, root) = match agent {
            AgentKind::Claude => (claude_ownership_state_path(paths), &paths.claude_home),
            AgentKind::Amp => (
                paths.home.join(".agent-sync/state/amp-resources.json"),
                &paths.amp_home,
            ),
            AgentKind::Codex => (
                paths.home.join(".agent-sync/state/codex-resources.json"),
                &paths.codex_home,
            ),
            AgentKind::Cursor => bail!("Cursor resources use their existing preservation policy"),
        };
        ensure_safe_state_path(&path)?;
        let original = read_optional_bytes(&path)?;
        let state = match original.as_deref() {
            Some(raw) => serde_json::from_slice(raw).with_context(|| {
                format!("parse Agent resource ownership state {}", path.display())
            })?,
            None => ResourceOwnershipState {
                schema_version: RESOURCE_OWNERSHIP_STATE_SCHEMA_VERSION,
                root: root.clone(),
                resources: BTreeMap::new(),
            },
        };
        validate_state(&state, root, &path)?;
        Ok(Self {
            path,
            original,
            state,
            agent,
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
        require_sha256(current_sha256, "current Agent resource")?;
        let identity = resource_identity(resource)?;
        let Some(owned) = self.state.resources.get(&identity) else {
            return Ok(false);
        };
        if owned.destination != destination {
            return Ok(false);
        }
        Ok(owned.installed_sha256 == current_sha256)
    }

    /// Records content that agent-sync just installed at an agent destination.
    pub(crate) fn record(
        &mut self,
        resource: &Resource,
        destination: PathBuf,
        installed_sha256: String,
    ) -> Result<()> {
        require_sha256(&installed_sha256, "installed Agent resource")?;
        validate_destination(&self.state.root, &destination, &self.path)?;
        let identity = resource_identity(resource)?;
        self.state.resources.insert(
            identity,
            OwnedResource {
                destination,
                installed_sha256,
            },
        );
        Ok(())
    }

    pub(crate) fn render(&self) -> Result<Vec<u8>> {
        validate_state(&self.state, &self.state.root, &self.path)?;
        let mut value = serde_json::to_value(&self.state)?;
        // Keep the existing Claude state format readable by older releases.
        if self.agent == AgentKind::Claude {
            let object = value
                .as_object_mut()
                .context("resource ownership must be an object")?;
            if let Some(root) = object.remove("root") {
                object.insert("claude_root".to_string(), root);
            }
        }
        Ok([serde_json::to_vec_pretty(&value)?, b"\n".to_vec()].concat())
    }
}

pub(crate) fn claude_ownership_state_path(paths: &AgentPaths) -> PathBuf {
    paths
        .home
        .join(".agent-sync")
        .join("state")
        .join(CLAUDE_OWNERSHIP_STATE_FILE)
}

fn validate_state(state: &ResourceOwnershipState, root: &Path, path: &Path) -> Result<()> {
    if state.schema_version != RESOURCE_OWNERSHIP_STATE_SCHEMA_VERSION {
        bail!(
            "unsupported Agent resource ownership state version {} in {}",
            state.schema_version,
            path.display()
        );
    }
    if state.root != root {
        bail!(
            "refusing to use Agent resource ownership state for {}; it records {}",
            root.display(),
            state.root.display()
        );
    }
    for (identity, owned) in &state.resources {
        validate_identity(identity, path)?;
        validate_destination(root, &owned.destination, path)?;
        require_sha256(
            &owned.installed_sha256,
            &format!("Agent resource `{identity}`"),
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
            "Agent resource ownership state has invalid identity `{identity}` in {}",
            path.display()
        );
    };
    if !matches!(kind, "skill" | "rule") || require_safe_name(name).is_err() {
        bail!(
            "Agent resource ownership state has invalid identity `{identity}` in {}",
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
        bail!("Agent resource ownership name `{name}` must be one path component");
    }
    Ok(())
}

fn validate_destination(root: &Path, destination: &Path, path: &Path) -> Result<()> {
    let relative = destination.strip_prefix(root).with_context(|| {
        format!(
            "Agent resource ownership destination {} is outside {} in {}",
            destination.display(),
            root.display(),
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
            "Agent resource ownership destination {} is invalid in {}",
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
            "refusing to read or replace symlinked Agent resource ownership state {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "Agent resource ownership state path {} is not a regular file",
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

        assert!(error.to_string().contains("symlinked Agent resource"));
    }
}

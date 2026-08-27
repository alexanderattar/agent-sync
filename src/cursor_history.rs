use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::{
    adapters::AgentPaths,
    fsx::{ensure_dir, read_to_string_if_exists, replace_file_with_backup, write_atomic},
};

const CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS: u64 = 300;
const MANAGED_HOOK_COMMAND_PREFIX: &str = "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 ";
const MANAGED_HOOK_COMMAND_SUFFIX: &str = " cursor-history export";
const QMD_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(45);
const QMD_REFRESH_LOCK_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorHistoryInstallReport {
    pub changed: bool,
    pub dry_run: bool,
    pub hooks_path: PathBuf,
    pub backup: Option<PathBuf>,
}

impl CursorHistoryInstallReport {
    pub fn to_text(&self) -> String {
        if self.changed && self.dry_run {
            format!(
                "Dry run. Add Cursor history hook -> {}\n",
                self.hooks_path.display()
            )
        } else if self.changed {
            match &self.backup {
                Some(backup) => format!(
                    "Added Cursor history hook -> {}\nBackup: {}\n",
                    self.hooks_path.display(),
                    backup.display()
                ),
                None => format!(
                    "Added Cursor history hook -> {}\n",
                    self.hooks_path.display()
                ),
            }
        } else {
            format!(
                "Unchanged Cursor history hook -> {}\n",
                self.hooks_path.display()
            )
        }
    }
}

pub fn install_cursor_history_hook(
    paths: &AgentPaths,
    executable: &Path,
    dry_run: bool,
) -> Result<CursorHistoryInstallReport> {
    let hooks_path = paths.cursor_home.join("hooks.json");
    let command = format!(
        "{MANAGED_HOOK_COMMAND_PREFIX}{}{MANAGED_HOOK_COMMAND_SUFFIX}",
        shell_quote(&executable.to_string_lossy())
    );
    let (content, changed) = render_cursor_hooks(&hooks_path, &command)?;
    if dry_run || !changed {
        return Ok(CursorHistoryInstallReport {
            changed,
            dry_run,
            hooks_path,
            backup: None,
        });
    }

    let backup_root = paths
        .home
        .join(".agent-sync")
        .join("backups")
        .join(Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    let backup = replace_file_with_backup(&backup_root, &paths.cursor_home, &hooks_path, &content)?;
    Ok(CursorHistoryInstallReport {
        changed,
        dry_run,
        hooks_path,
        backup,
    })
}

pub fn export_cursor_history_from_stdin(
    paths: &AgentPaths,
    output_dir: Option<PathBuf>,
    refresh_qmd: bool,
) -> Result<Option<PathBuf>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let hook: Value = serde_json::from_str(input.trim()).context("parse Cursor hook input")?;
    export_cursor_history(paths, &hook, output_dir, refresh_qmd)
}

pub fn export_cursor_history(
    paths: &AgentPaths,
    hook: &Value,
    output_dir: Option<PathBuf>,
    refresh_qmd: bool,
) -> Result<Option<PathBuf>> {
    let Some(transcript_path) = hook.get("transcript_path").and_then(Value::as_str) else {
        return Ok(None);
    };
    let transcript_path = PathBuf::from(transcript_path);
    let transcript_path = transcript_path
        .canonicalize()
        .with_context(|| format!("resolve {}", transcript_path.display()))?;
    let cursor_projects = paths.cursor_home.join("projects");
    let cursor_projects = cursor_projects
        .canonicalize()
        .with_context(|| format!("resolve {}", cursor_projects.display()))?;
    if !transcript_path.starts_with(&cursor_projects) {
        anyhow::bail!(
            "Cursor transcript {} is outside {}",
            transcript_path.display(),
            cursor_projects.display()
        );
    }

    let raw = fs::read_to_string(&transcript_path)
        .with_context(|| format!("read {}", transcript_path.display()))?;
    let turns = cursor_turns(&raw);
    if turns.is_empty() {
        return Ok(None);
    }

    let conversation_id = hook
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            transcript_path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
        })
        .context("Cursor hook input has no conversation id")?;
    let modified = fs::metadata(&transcript_path)?.modified()?;
    let timestamp: DateTime<Utc> = modified.into();
    let output_dir = output_dir.unwrap_or_else(|| {
        paths
            .home
            .join("Documents")
            .join("Obsidian")
            .join("sessions")
    });
    fs::create_dir_all(&output_dir).with_context(|| format!("create {}", output_dir.display()))?;
    let output = output_dir.join(format!("cursor-{}.md", safe_filename(&conversation_id)));
    let workspace_roots = hook
        .get("workspace_roots")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let model = hook.get("model").and_then(Value::as_str).unwrap_or("");
    let content = render_markdown(&conversation_id, timestamp, model, &workspace_roots, &turns)?;

    let changed = read_to_string_if_exists(&output)?.as_deref() != Some(content.as_str());
    if changed {
        write_atomic(&output, content.as_bytes())?;
    }
    if changed && refresh_qmd {
        refresh_qmd_index(paths)?;
    }
    Ok(Some(output))
}

fn render_cursor_hooks(path: &Path, command: &str) -> Result<(Vec<u8>, bool)> {
    let mut root: Value = match read_to_string_if_exists(path)? {
        Some(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parse Cursor hooks file {}", path.display()))?,
        None => json!({}),
    };
    let object = root
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    object.entry("version".to_string()).or_insert(json!(1));
    let hooks = object
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("{}.hooks must be a JSON object", path.display()))?;
    let stop = hooks
        .entry("stop".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .with_context(|| format!("{}.hooks.stop must be a JSON array", path.display()))?;
    let managed = stop
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_managed_hook(entry))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if managed.len() > 1 {
        anyhow::bail!(
            "{}.hooks.stop contains more than one agent-sync-managed Cursor history hook",
            path.display()
        );
    }
    if let Some(index) = managed.first().copied() {
        let entry = stop[index]
            .as_object_mut()
            .context("agent-sync-managed Cursor history hook must be a JSON object")?;
        let command_matches = entry.get("command").and_then(Value::as_str) == Some(command);
        let timeout_matches = entry.get("timeout").and_then(Value::as_u64)
            == Some(CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS);
        if command_matches && timeout_matches {
            return Ok((
                [serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat(),
                false,
            ));
        }
        entry.insert("command".to_string(), json!(command));
        entry.insert(
            "timeout".to_string(),
            json!(CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS),
        );
        return Ok((
            [serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat(),
            true,
        ));
    }
    stop.push(json!({
        "command": command,
        "timeout": CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS
    }));
    Ok((
        [serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat(),
        true,
    ))
}

fn cursor_turns(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|entry| {
            let role = entry.get("role")?.as_str()?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let content = entry
                .get("message")?
                .get("content")?
                .as_array()?
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if content.trim().is_empty() {
                None
            } else {
                Some((role.to_string(), content))
            }
        })
        .collect()
}

fn render_markdown(
    conversation_id: &str,
    timestamp: DateTime<Utc>,
    model: &str,
    workspace_roots: &[String],
    turns: &[(String, String)],
) -> Result<String> {
    let mut out = String::new();
    out.push_str("---\nsource: cursor\n");
    out.push_str(&format!(
        "conversation_id: {}\n",
        serde_json::to_string(conversation_id)?
    ));
    out.push_str(&format!("date: {}\n", timestamp.to_rfc3339()));
    out.push_str(&format!("model: {}\n", serde_json::to_string(model)?));
    out.push_str(&format!(
        "workspace_roots: {}\n---\n\n",
        serde_json::to_string(workspace_roots)?
    ));
    out.push_str(&format!("# Cursor session `{conversation_id}`\n\n"));
    for (role, text) in turns {
        let heading = if role == "user" { "User" } else { "Cursor" };
        out.push_str(&format!("## {heading}\n\n{text}\n\n"));
    }
    Ok(out)
}

fn is_managed_hook(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| command.strip_prefix(MANAGED_HOOK_COMMAND_PREFIX))
        .and_then(|command| command.strip_suffix(MANAGED_HOOK_COMMAND_SUFFIX))
        .is_some_and(|executable| !executable.is_empty())
}

fn refresh_qmd_index(paths: &AgentPaths) -> Result<()> {
    let Some(_lock) = acquire_qmd_refresh_lock(paths)? else {
        return Ok(());
    };
    if qmd_refresh_is_throttled(paths, SystemTime::now())? {
        return Ok(());
    }

    let candidates = [
        paths.home.join(".local/bin/qmd"),
        PathBuf::from("/usr/local/bin/qmd"),
        PathBuf::from("/opt/homebrew/bin/qmd"),
    ];
    let qmd = candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .unwrap_or_else(|| PathBuf::from("qmd"));
    run_qmd_command(&qmd, "update")?;
    run_qmd_command(&qmd, "embed")?;
    write_qmd_refresh_state(paths, SystemTime::now())
}

fn run_qmd_command(qmd: &Path, subcommand: &str) -> Result<()> {
    let status = Command::new(qmd)
        .arg(subcommand)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run qmd {subcommand}"))?;
    if !status.success() {
        anyhow::bail!("qmd {subcommand} failed with {status}");
    }
    Ok(())
}

struct QmdRefreshLock {
    path: PathBuf,
}

impl Drop for QmdRefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_qmd_refresh_lock(paths: &AgentPaths) -> Result<Option<QmdRefreshLock>> {
    let state_dir = qmd_refresh_state_dir(paths);
    ensure_dir(&state_dir)?;
    let path = state_dir.join("qmd-refresh.lock");

    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let lock = QmdRefreshLock { path: path.clone() };
                writeln!(file, "{}", std::process::id())
                    .with_context(|| format!("write QMD refresh lock {}", path.display()))?;
                return Ok(Some(lock));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= QMD_REFRESH_LOCK_STALE_AFTER);
                if !stale {
                    return Ok(None);
                }
                match fs::remove_file(&path) {
                    Ok(()) => continue,
                    Err(error) if error.kind() == ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("remove stale QMD refresh lock {}", path.display())
                        });
                    }
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create QMD refresh lock {}", path.display()));
            }
        }
    }
    Ok(None)
}

fn qmd_refresh_is_throttled(paths: &AgentPaths, now: SystemTime) -> Result<bool> {
    let state_path = qmd_refresh_state_dir(paths).join("qmd-refresh-state.json");
    let Some(raw) = read_to_string_if_exists(&state_path)? else {
        return Ok(false);
    };
    let state: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse QMD refresh state {}", state_path.display()))?;
    let Some(last_success_millis) = state.get("lastSuccessUnixMillis").and_then(Value::as_u64)
    else {
        return Ok(false);
    };
    let now_millis = unix_millis(now)?;
    Ok(
        now_millis.saturating_sub(last_success_millis)
            < QMD_REFRESH_MIN_INTERVAL.as_millis() as u64,
    )
}

fn write_qmd_refresh_state(paths: &AgentPaths, now: SystemTime) -> Result<()> {
    let state_path = qmd_refresh_state_dir(paths).join("qmd-refresh-state.json");
    let content = [
        serde_json::to_vec_pretty(&json!({
            "lastSuccessUnixMillis": unix_millis(now)?,
        }))?,
        b"\n".to_vec(),
    ]
    .concat();
    write_atomic(&state_path, &content)
}

fn qmd_refresh_state_dir(paths: &AgentPaths) -> PathBuf {
    paths.home.join(".agent-sync").join("state")
}

fn unix_millis(time: SystemTime) -> Result<u64> {
    time.duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .try_into()
        .context("Unix timestamp does not fit in u64")
}

fn safe_filename(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MANAGED_COMMAND: &str =
        "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/opt/agent-sync' cursor-history export";

    #[test]
    fn cursor_hook_merge_preserves_existing_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hooks.json");
        fs::write(
            &path,
            r#"{"version":1,"custom":true,"hooks":{"stop":[{"command":"existing"}],"afterFileEdit":[{"command":"format"}]}}"#,
        )
        .unwrap();

        let (content, changed) = render_cursor_hooks(&path, TEST_MANAGED_COMMAND).unwrap();
        assert!(changed);
        let value: Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(value["custom"], true);
        assert_eq!(value["hooks"]["stop"][0]["command"], "existing");
        assert_eq!(value["hooks"]["stop"][1]["command"], TEST_MANAGED_COMMAND);
        assert_eq!(
            value["hooks"]["stop"][1]["timeout"],
            CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS
        );
        assert_eq!(value["hooks"]["afterFileEdit"][0]["command"], "format");
    }

    #[test]
    fn cursor_hook_repairs_only_the_managed_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hooks.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "hooks": {
    "stop": [
      {"command": "custom cursor-history export", "timeout": 17},
      {
        "command": "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/custom' cursor-history export --custom",
        "timeout": 19
      },
      {
        "command": "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/old/agent-sync' cursor-history export",
        "timeout": 30,
        "failClosed": false
      }
    ]
  }
}"#,
        )
        .unwrap();

        let (content, changed) = render_cursor_hooks(&path, TEST_MANAGED_COMMAND).unwrap();
        assert!(changed);
        let value: Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(
            value["hooks"]["stop"][0],
            json!({"command": "custom cursor-history export", "timeout": 17})
        );
        assert_eq!(
            value["hooks"]["stop"][1],
            json!({
                "command": "env AGENT_SYNC_CURSOR_HISTORY_HOOK=1 '/custom' cursor-history export --custom",
                "timeout": 19
            })
        );
        assert_eq!(value["hooks"]["stop"][2]["command"], TEST_MANAGED_COMMAND);
        assert_eq!(
            value["hooks"]["stop"][2]["timeout"],
            CURSOR_HISTORY_HOOK_TIMEOUT_SECONDS
        );
        assert_eq!(value["hooks"]["stop"][2]["failClosed"], false);
    }

    #[test]
    fn cursor_transcript_exports_to_searchable_markdown() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = paths
            .cursor_home
            .join("projects/example/agent-transcripts/session/session.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            concat!(
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
                "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\"},{\"type\":\"text\",\"text\":\"hi\"}]}}\n"
            ),
        )
        .unwrap();
        let output_dir = temp.path().join("sessions");
        let hook = json!({
            "conversation_id": "session",
            "model": "cursor-model",
            "workspace_roots": ["/example"],
            "transcript_path": transcript,
        });

        let output = export_cursor_history(&paths, &hook, Some(output_dir), false)
            .unwrap()
            .unwrap();
        let markdown = fs::read_to_string(output).unwrap();
        assert!(markdown.contains("source: cursor"));
        assert!(markdown.contains("## User\n\nhello"));
        assert!(markdown.contains("## Cursor\n\nhi"));
        assert!(!markdown.contains("tool_use"));
    }

    #[cfg(unix)]
    #[test]
    fn cursor_history_refreshes_qmd_only_after_markdown_changes() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let output_dir = temp.path().join("sessions");
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, None);
        let hook = test_hook(&transcript);

        export_cursor_history(&paths, &hook, Some(output_dir.clone()), true).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "update\nembed\n");

        export_cursor_history(&paths, &hook, Some(output_dir), true).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "update\nembed\n");
    }

    #[cfg(unix)]
    #[test]
    fn cursor_history_propagates_qmd_command_failures() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        let transcript = write_test_transcript(&paths);
        let log = temp.path().join("qmd.log");
        install_test_qmd(&paths, &log, Some("embed"));

        let error = export_cursor_history(
            &paths,
            &test_hook(&transcript),
            Some(temp.path().join("sessions")),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("qmd embed failed"));
        assert_eq!(fs::read_to_string(log).unwrap(), "update\nembed\n");
    }

    #[test]
    fn qmd_refresh_lock_and_state_are_shared() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());

        let lock = acquire_qmd_refresh_lock(&paths).unwrap().unwrap();
        assert!(acquire_qmd_refresh_lock(&paths).unwrap().is_none());
        drop(lock);
        assert!(acquire_qmd_refresh_lock(&paths).unwrap().is_some());

        write_qmd_refresh_state(&paths, SystemTime::now()).unwrap();
        assert!(qmd_refresh_is_throttled(&paths, SystemTime::now()).unwrap());
    }

    fn write_test_transcript(paths: &AgentPaths) -> PathBuf {
        let transcript = paths
            .cursor_home
            .join("projects/example/agent-transcripts/session/session.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            concat!(
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
                "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n"
            ),
        )
        .unwrap();
        transcript
    }

    fn test_hook(transcript: &Path) -> Value {
        json!({
            "conversation_id": "session",
            "model": "cursor-model",
            "workspace_roots": ["/example"],
            "transcript_path": transcript,
        })
    }

    #[cfg(unix)]
    fn install_test_qmd(paths: &AgentPaths, log: &Path, fail_on: Option<&str>) {
        use std::os::unix::fs::PermissionsExt;

        let qmd = paths.home.join(".local/bin/qmd");
        fs::create_dir_all(qmd.parent().unwrap()).unwrap();
        let failure = fail_on
            .map(|subcommand| format!("[ \"$1\" = \"{subcommand}\" ] && exit 9\n"))
            .unwrap_or_default();
        fs::write(
            &qmd,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" >> {}\n{failure}exit 0\n",
                shell_quote(&log.to_string_lossy())
            ),
        )
        .unwrap();
        fs::set_permissions(&qmd, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

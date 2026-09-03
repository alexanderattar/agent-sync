use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn bin() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agent-sync"));
    if path.is_absolute() {
        path
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
    }
}

fn run(root: &Path, args: &[&str]) -> String {
    let output = run_output(root, args);

    if !output.status.success() {
        panic!(
            "agent-sync failed\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    String::from_utf8(output.stdout).expect("stdout utf8")
}

fn run_failure(root: &Path, args: &[&str]) -> String {
    let output = run_output(root, args);
    assert!(
        !output.status.success(),
        "agent-sync unexpectedly succeeded\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn run_output(root: &Path, args: &[&str]) -> Output {
    let output = Command::new(bin())
        .env("HOME", root)
        .env("AGENT_SYNC_TEST_DISABLE_BACKGROUND", "1")
        .args([
            "--home",
            root.to_str().unwrap(),
            "--codex-home",
            root.join(".codex").to_str().unwrap(),
            "--claude-home",
            root.join(".claude").to_str().unwrap(),
            "--claude-config",
            root.join(".claude.json").to_str().unwrap(),
            "--cursor-home",
            root.join(".cursor").to_str().unwrap(),
            "--cursor-config",
            root.join(".cursor/mcp.json").to_str().unwrap(),
            "--agents-home",
            root.join(".agents").to_str().unwrap(),
        ])
        .args(args)
        .output()
        .expect("run agent-sync");
    output
}

fn setup_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    write(
        &root.join(".codex/skills/pr-review/SKILL.md"),
        r#"---
name: pr-review
description: Review PRs
---

# PR Review
"#,
    );
    write(
        &root.join(".agents/skills/shared-style/SKILL.md"),
        r#"---
name: shared-style
description: Shared style rules
---

# Shared Style
"#,
    );
    write(
        &root.join(".codex/AGENTS.md"),
        "# Global Agent Rules\n\n- Keep changes scoped.\n",
    );
    write(
        &root.join(".codex/config.toml"),
        r#"
[mcp_servers.qmd]
command = "/usr/local/bin/qmd"
args = ["mcp"]

[mcp_servers.example_http]
url = "https://mcp.example.invalid/mcp"

[mcp_servers.example_http.env_http_headers]
Authorization = "EXAMPLE_MCP_AUTHORIZATION"
"#,
    );
    write(
        &root.join(".codex/memories/memory_summary.md"),
        "memory summary\n",
    );
    write(&root.join(".codex/memories/MEMORY.md"), "memory index\n");
    write(
        &root.join(".codex/automations/check/automation.toml"),
        "name = \"check\"\n",
    );

    fs::create_dir_all(root.join(".claude")).unwrap();
    write(&root.join(".claude.json"), "{}\n");
    write(&root.join(".cursor/mcp.json"), "{\"mcpServers\": {}}\n");

    temp
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn assert_managed_cursor_rule_marker(content: &str) {
    let frontmatter = "---\ndescription: Imported Codex agent guidance\nalwaysApply: true\n---\n";
    let marked_body = content
        .strip_prefix(frontmatter)
        .expect("managed Cursor rule frontmatter");
    let (marker, body) = marked_body.split_once('\n').expect("managed marker line");
    let expected = marker
        .strip_prefix("<!-- agent-sync-managed: cursor-codex-agents body-sha256=")
        .and_then(|value| value.strip_suffix(" -->"))
        .expect("managed marker hash");
    let actual = format!("{:x}", Sha256::digest(body.as_bytes()));
    assert_eq!(actual, expected, "{content}");
}

#[test]
fn exports_applies_and_verifies_codex_pack_to_claude() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("pack");

    let export = run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
        ],
    );
    assert!(export.contains("Exported"));
    assert!(pack.join("agent-sync.manifest.json").exists());
    assert!(pack.join("skills/pr-review/SKILL.md").exists());
    assert!(pack.join("skills/shared-style/SKILL.md").exists());
    assert!(pack.join("references/codex-memories/MEMORY.md").exists());
    assert!(pack
        .join("references/codex-automations/check/automation.toml")
        .exists());

    let dry_run = run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
        ],
    );
    assert!(dry_run.contains("Dry run"));
    assert!(!root.join(".claude/skills/pr-review/SKILL.md").exists());

    let applied = run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
            "--yes",
        ],
    );
    assert!(applied.contains("Applied changes"));
    assert!(applied.contains("Add claude Skill:pr-review"));
    assert!(applied.contains("Add claude Mcp:"));
    assert!(root.join(".claude/skills/pr-review/SKILL.md").exists());
    assert!(root.join(".claude/skills/shared-style/SKILL.md").exists());
    let backup_dirs: Vec<PathBuf> = fs::read_dir(root.join(".agent-sync/backups"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(backup_dirs.len(), 1);
    assert!(backup_dirs[0].join(".claude.json").exists());

    let imported_rule =
        fs::read_to_string(root.join(".claude/rules/imported-codex-agents.md")).unwrap();
    assert!(imported_rule.contains("Imported Codex Agent Rules"));
    assert!(imported_rule.contains("Keep changes scoped."));

    let claude_json: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(
        claude_json["mcpServers"]["qmd"]["command"],
        "/usr/local/bin/qmd"
    );
    assert_eq!(
        claude_json["mcpServers"]["example_http"]["headers"]["Authorization"],
        "${EXAMPLE_MCP_AUTHORIZATION}"
    );

    let verify = run(
        root,
        &[
            "verify",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
        ],
    );
    assert!(verify.contains("Verification passed"));

    let post_apply_diff = run(
        root,
        &[
            "diff",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
        ],
    );
    assert!(post_apply_diff.contains("Unchanged claude Rule:codex-agents"));
    assert!(!post_apply_diff.contains("Update"));
}

#[test]
fn claude_source_prefers_claude_owned_skill_over_shared_duplicate() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("claude-source-pack");
    write(
        &root.join(".agents/skills/foo/SKILL.md"),
        "---\nname: foo\ndescription: Shared copy\n---\n\n# Shared foo\n",
    );
    write(
        &root.join(".claude/skills/foo/SKILL.md"),
        "---\nname: foo\ndescription: Claude copy\n---\n\n# Claude foo\n",
    );

    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "claude",
        ],
    );

    let exported = fs::read_to_string(pack.join("skills/foo/SKILL.md")).unwrap();
    assert!(exported.contains("# Claude foo"));
    assert!(!exported.contains("# Shared foo"));
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(pack.join("agent-sync.manifest.json")).unwrap())
            .unwrap();
    let foo = manifest["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["name"] == "foo")
        .unwrap();
    assert_eq!(foo["source_agent"], "claude");
}

#[test]
fn discover_reports_codex_claude_and_shared_agent_sources() {
    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".claude/skills/claude-only/SKILL.md"),
        "---\nname: claude-only\ndescription: Claude only\n---\n",
    );

    let output = run(root, &["discover"]);
    assert!(output.contains("Codex"));
    assert!(output.contains("pr-review"));
    assert!(output.contains("Claude"));
    assert!(output.contains("claude-only"));
    assert!(output.contains("Cursor"));
    assert!(output.contains("Shared .agents"));
    assert!(output.contains("shared-style"));
}

#[test]
fn cursor_reuses_supported_codex_and_shared_skill_locations() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("cursor-compatible-skills");

    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    let dry_run = run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );

    assert!(dry_run.contains("Unchanged cursor Skill:pr-review"));
    assert!(dry_run.contains(root.join(".codex/skills/pr-review").to_str().unwrap()));
    assert!(dry_run.contains("Unchanged cursor Skill:shared-style"));
    assert!(dry_run.contains(root.join(".agents/skills/shared-style").to_str().unwrap()));
    assert!(!root.join(".cursor/skills/pr-review").exists());
    assert!(!root.join(".cursor/skills/shared-style").exists());
}

#[test]
fn applies_pack_to_cursor_additively_and_preserves_cursor_state() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("cursor-pack");

    write(
        &root.join(".codex/skills/missing-in-cursor/SKILL.md"),
        "---\nname: missing-in-cursor\ndescription: Portable skill\n---\n",
    );
    let cursor_skill = "---\nname: pr-review\ndescription: Cursor-specific review\n---\n";
    write(
        &root.join(".cursor/skills/pr-review/SKILL.md"),
        cursor_skill,
    );
    let cursor_rule =
        "---\ndescription: Cursor-owned import\nalwaysApply: true\n---\nCursor wins.\n";
    write(
        &root.join(".cursor/rules/imported-codex-agents.mdc"),
        cursor_rule,
    );
    write(
        &root.join(".cursor/rules/cursor-only.mdc"),
        "---\nalwaysApply: true\n---\nCursor only.\n",
    );
    write(
        &root.join(".cursor/skills-cursor/managed/SKILL.md"),
        "managed sentinel\n",
    );
    write(
        &root.join(".cursor/plugins/local/sentinel/plugin.json"),
        "{\"name\":\"sentinel\"}\n",
    );
    let cursor_mcp = r#"{
  "cursorSetting": {"keep": true},
  "mcpServers": {
    "qmd": {
      "url": "http://cursor-specific.invalid/mcp",
      "cursorOnlyField": true
    },
    "cursor-only": {
      "command": "cursor-tool",
      "args": ["serve"]
    }
  }
}
"#;
    write(&root.join(".cursor/mcp.json"), cursor_mcp);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            root.join(".cursor/mcp.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    assert!(!pack.join("references/codex-memories").exists());
    assert!(!pack.join("references/codex-automations").exists());
    fs::remove_dir_all(root.join(".codex/skills/missing-in-cursor")).unwrap();

    let dry_run = run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(dry_run.contains("Skip cursor Skill:pr-review"));
    assert!(dry_run.contains("Add cursor Skill:missing-in-cursor"));
    assert!(dry_run.contains("Skip cursor Rule:codex-agents"));
    assert!(dry_run.contains("Skip cursor Mcp:qmd"));
    assert!(dry_run.contains("Add cursor Mcp:"));

    run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );

    assert_eq!(
        fs::read_to_string(root.join(".cursor/skills/pr-review/SKILL.md")).unwrap(),
        cursor_skill
    );
    assert!(root
        .join(".cursor/skills/missing-in-cursor/SKILL.md")
        .exists());
    assert_eq!(
        fs::read_to_string(root.join(".cursor/rules/imported-codex-agents.mdc")).unwrap(),
        cursor_rule
    );
    assert!(root.join(".cursor/rules/cursor-only.mdc").exists());
    assert!(root.join(".cursor/skills-cursor/managed/SKILL.md").exists());
    assert!(root
        .join(".cursor/plugins/local/sentinel/plugin.json")
        .exists());

    let merged_raw = fs::read_to_string(root.join(".cursor/mcp.json")).unwrap();
    let insertion_at = cursor_mcp.rfind("}\n}\n").unwrap();
    assert_eq!(
        &merged_raw.as_bytes()[..insertion_at],
        &cursor_mcp.as_bytes()[..insertion_at]
    );
    assert!(merged_raw.ends_with(&cursor_mcp[insertion_at..]));
    let insertion_end = merged_raw.len() - (cursor_mcp.len() - insertion_at);
    assert!(merged_raw[insertion_at..insertion_end].starts_with(",\"example_http\":"));

    let merged: Value = serde_json::from_str(&merged_raw).unwrap();
    assert_eq!(merged["cursorSetting"]["keep"], true);
    assert_eq!(
        merged["mcpServers"]["qmd"]["url"],
        "http://cursor-specific.invalid/mcp"
    );
    assert_eq!(merged["mcpServers"]["qmd"]["cursorOnlyField"], true);
    assert_eq!(
        merged["mcpServers"]["cursor-only"]["command"],
        "cursor-tool"
    );
    assert_eq!(
        merged["mcpServers"]["example_http"]["url"],
        "https://mcp.example.invalid/mcp"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.join(".cursor/mcp.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let verify = run(
        root,
        &[
            "verify",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(verify.contains("Verification passed"));

    let second_diff = run(
        root,
        &[
            "diff",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(!second_diff.contains("Add cursor"));
    assert!(!second_diff.contains("Update cursor"));
}

#[test]
fn managed_cursor_mcp_entry_updates_from_recorded_state() {
    let temp = setup_fixture();
    let root = temp.path();
    let initial_pack = root.join("cursor-mcp-initial-pack");
    let updated_pack = root.join("cursor-mcp-updated-pack");

    run(
        root,
        &[
            "export",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    run(
        root,
        &[
            "apply",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    assert!(root.join(".agent-sync/state/cursor-mcp.json").exists());

    write(
        &root.join(".codex/config.toml"),
        "[mcp_servers.qmd]\ncommand = \"/opt/qmd-v2\"\nargs = [\"mcp\", \"--new\"]\n",
    );
    run(
        root,
        &[
            "export",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    let preview = run(
        root,
        &[
            "apply",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(
        preview.contains("ManagedUpdate cursor Mcp:qmd"),
        "{preview}"
    );

    run(
        root,
        &[
            "apply",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    let cursor: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert_eq!(cursor["mcpServers"]["qmd"]["command"], "/opt/qmd-v2");
    assert_eq!(
        cursor["mcpServers"]["qmd"]["args"],
        serde_json::json!(["mcp", "--new"])
    );
}

#[test]
fn cursor_mcp_user_edit_is_preserved_after_source_changes() {
    let temp = setup_fixture();
    let root = temp.path();
    let initial_pack = root.join("cursor-mcp-owned-pack");
    let updated_pack = root.join("cursor-mcp-user-edit-pack");

    run(
        root,
        &[
            "export",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    run(
        root,
        &[
            "apply",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );

    let cursor_path = root.join(".cursor/mcp.json");
    let mut cursor: Value =
        serde_json::from_str(&fs::read_to_string(&cursor_path).unwrap()).unwrap();
    cursor["mcpServers"]["qmd"]["cursorOnlyField"] = serde_json::json!(true);
    write(
        &cursor_path,
        &format!("{}\n", serde_json::to_string_pretty(&cursor).unwrap()),
    );
    let user_value = cursor["mcpServers"]["qmd"].clone();

    write(
        &root.join(".codex/config.toml"),
        "[mcp_servers.qmd]\ncommand = \"/opt/qmd-v2\"\nargs = [\"mcp\"]\n",
    );
    run(
        root,
        &[
            "export",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    let preview = run(
        root,
        &[
            "apply",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(preview.contains("Skip cursor Mcp:qmd"), "{preview}");

    run(
        root,
        &[
            "apply",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    let preserved: Value = serde_json::from_str(&fs::read_to_string(cursor_path).unwrap()).unwrap();
    assert_eq!(preserved["mcpServers"]["qmd"], user_value);
}

#[cfg(unix)]
#[test]
fn cursor_mcp_state_write_failure_rolls_back_config() {
    use std::os::unix::fs::PermissionsExt;

    let temp = setup_fixture();
    let root = temp.path();
    let initial_pack = root.join("cursor-mcp-rollback-initial-pack");
    let updated_pack = root.join("cursor-mcp-rollback-updated-pack");

    run(
        root,
        &[
            "export",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    run(
        root,
        &[
            "apply",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    write(
        &root.join(".codex/config.toml"),
        "[mcp_servers.qmd]\ncommand = \"/opt/qmd-v2\"\nargs = [\"mcp\"]\n",
    );
    run(
        root,
        &[
            "export",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    let preview = run(
        root,
        &[
            "apply",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(preview.contains("ManagedUpdate cursor Mcp:qmd"));

    let cursor_path = root.join(".cursor/mcp.json");
    let cursor_before = fs::read(&cursor_path).unwrap();
    let state_dir = root.join(".agent-sync/state");
    let original_permissions = fs::metadata(&state_dir).unwrap().permissions();
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let output = run_output(
        root,
        &[
            "apply",
            "--pack",
            updated_pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    fs::set_permissions(&state_dir, original_permissions).unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Cursor MCP ownership state"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(cursor_path).unwrap(), cursor_before);
}

#[test]
fn cursor_rule_embeds_codex_guidance_and_preserves_cursor_precedence() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("cursor-rule-pack");
    write(
        &root.join(".codex/AGENTS.md"),
        "# Global Agent Rules\n\ncredential-bearing-sentinel\n",
    );

    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );

    let imported =
        fs::read_to_string(root.join(".cursor/rules/imported-codex-agents.mdc")).unwrap();
    assert!(imported.contains("Cursor-specific settings and rules take precedence"));
    assert!(imported.contains("credential-bearing-sentinel"));
}

#[test]
fn managed_cursor_rule_updates_until_a_user_edits_it() {
    let temp = setup_fixture();
    let root = temp.path();
    let rule = root.join(".cursor/rules/imported-codex-agents.mdc");

    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);
    let initial = fs::read_to_string(&rule).unwrap();
    assert!(initial.contains("agent-sync-managed: cursor-codex-agents body-sha256="));
    assert_managed_cursor_rule_marker(&initial);
    assert!(initial.contains("Keep changes scoped."));

    write(
        &root.join(".codex/AGENTS.md"),
        "# Global Agent Rules\n\n- Keep changes scoped.\n- Use focused tests.\n",
    );
    let update_pack = root.join("managed-rule-update-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            update_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    let direct_diff = run_output(
        root,
        &[
            "diff",
            "--pack",
            update_pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(direct_diff.status.success());
    assert!(
        String::from_utf8_lossy(&direct_diff.stdout)
            .contains("ManagedUpdate cursor Rule:codex-agents"),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&direct_diff.stdout),
        String::from_utf8_lossy(&direct_diff.stderr)
    );
    let update_preview = run(root, &["sync"]);
    assert!(
        update_preview.contains("ManagedUpdate cursor Rule:codex-agents"),
        "{update_preview}"
    );

    run(root, &["sync", "--yes"]);
    let updated = fs::read_to_string(&rule).unwrap();
    assert_ne!(updated, initial);
    assert!(updated.contains("Use focused tests."));

    let manually_edited = format!("{updated}\nCursor-owned addition.\n");
    write(&rule, &manually_edited);
    let preserve_diff = run(
        root,
        &[
            "diff",
            "--pack",
            update_pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(
        preserve_diff.contains("Skip cursor Rule:codex-agents"),
        "{preserve_diff}"
    );
    let preserve_preview = run(root, &["sync"]);
    assert!(
        preserve_preview.contains("Preserved target-owned resources:"),
        "{preserve_preview}"
    );

    run(root, &["sync", "--yes"]);
    assert_eq!(fs::read_to_string(rule).unwrap(), manually_edited);
}

#[test]
fn exact_legacy_cursor_rule_upgrades_but_an_edited_copy_is_preserved() {
    const LEGACY_RULE: &str = "---\ndescription: Bridge to Codex global agent rules\nalwaysApply: true\n---\n# Codex Agent Rule Bridge\n\nBefore starting a task, read and follow `~/.codex/AGENTS.md` when it exists. Treat it as shared guidance. Direct user instructions and Cursor-specific settings and rules take precedence if they conflict with that file.\n\nWhen prior work may matter, search the QMD `sessions` collection. QMD history is searchable context, not a resumable Cursor chat.\n";

    let temp = setup_fixture();
    let root = temp.path();
    let rule = root.join(".cursor/rules/imported-codex-agents.mdc");
    run(root, &["setup", "--yes"]);

    write(&rule, LEGACY_RULE);
    let upgrade_preview = run(root, &["sync"]);
    assert!(
        upgrade_preview.contains("ManagedUpdate cursor Rule:codex-agents"),
        "{upgrade_preview}"
    );
    run(root, &["sync", "--yes"]);
    let upgraded = fs::read_to_string(&rule).unwrap();
    assert!(upgraded.contains("agent-sync-managed: cursor-codex-agents body-sha256="));
    assert!(upgraded.contains("Keep changes scoped."));

    let edited_legacy = format!("{LEGACY_RULE}Cursor-owned addition.\n");
    write(&rule, &edited_legacy);
    let preserve_preview = run(root, &["sync"]);
    assert!(
        preserve_preview.contains("Preserved target-owned resources:"),
        "{preserve_preview}"
    );
    run(root, &["sync", "--yes"]);
    assert_eq!(fs::read_to_string(rule).unwrap(), edited_legacy);
}

#[test]
fn cursor_mcp_rejects_malformed_server_map_without_rewriting_it() {
    let temp = setup_fixture();
    let root = temp.path();
    let malformed = "{\"mcpServers\": []}\n";
    write(&root.join(".cursor/mcp.json"), malformed);

    let output = Command::new(bin())
        .env("HOME", root)
        .args([
            "--home",
            root.to_str().unwrap(),
            "--cursor-home",
            root.join(".cursor").to_str().unwrap(),
            "--cursor-config",
            root.join(".cursor/mcp.json").to_str().unwrap(),
            "discover",
        ])
        .output()
        .expect("run agent-sync");

    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(root.join(".cursor/mcp.json")).unwrap(),
        malformed
    );
}

#[test]
fn cursor_custom_same_name_mcp_is_preserved_as_target_owned() {
    let temp = setup_fixture();
    let root = temp.path();
    let original = r#"{
  "mcpServers": {
    "qmd": {
      "provider": "cursor-native",
      "enabled": true
    }
  },
  "cursorSpecific": true
}
"#;
    write(&root.join(".cursor/mcp.json"), original);
    let pack = root.join("custom-cursor-mcp-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--mcp-servers",
            "qmd",
        ],
    );

    let preview = run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(preview.contains("Skip cursor Mcp:qmd"));
    run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    assert_eq!(
        fs::read_to_string(root.join(".cursor/mcp.json")).unwrap(),
        original
    );
    let verified = run(
        root,
        &[
            "verify",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(verified.contains("Verification passed"));
}

#[cfg(unix)]
#[test]
fn cursor_mcp_refuses_a_symlink_instead_of_replacing_it() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("cursor-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );

    let config = root.join(".cursor/mcp.json");
    let shared_config = root.join("shared-cursor/mcp.json");
    write(&shared_config, "{\"mcpServers\": {}}\n");
    fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(&shared_config, &config).unwrap();

    let error = run_failure(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    assert!(error.contains("refusing to rewrite symlinked Cursor MCP config"));
    assert!(fs::symlink_metadata(&config)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(&shared_config).unwrap(),
        "{\"mcpServers\": {}}\n"
    );
}

#[test]
fn cursor_project_mcp_names_win_without_being_added_to_mcp_json() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("cursor-pack");
    let project_mcp = root.join(".cursor/projects/example/mcps/plugin-qmd-provider");
    fs::create_dir_all(&project_mcp).unwrap();
    write(
        &project_mcp.join("SERVER_METADATA.json"),
        "{\"serverName\":\"qmd\"}\n",
    );
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );

    let original = fs::read_to_string(root.join(".cursor/mcp.json")).unwrap();
    let dry_run = run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(dry_run.contains("Skip cursor Mcp:qmd"));
    assert!(dry_run.contains("Add cursor Mcp:example_http"));

    run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    let updated = fs::read_to_string(root.join(".cursor/mcp.json")).unwrap();
    assert_ne!(updated, original);
    let updated: Value = serde_json::from_str(&updated).unwrap();
    assert!(updated["mcpServers"].get("qmd").is_none());
    assert_eq!(
        updated["mcpServers"]["example_http"]["url"],
        "https://mcp.example.invalid/mcp"
    );
    let verify = run(
        root,
        &[
            "verify",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(verify.contains("Verification passed"));
}

#[cfg(unix)]
#[test]
fn cursor_dangling_rule_symlink_is_preserved() {
    use std::os::unix::fs::symlink;

    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("cursor-rule-symlink-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    let rule = root.join(".cursor/rules/imported-codex-agents.mdc");
    fs::create_dir_all(rule.parent().unwrap()).unwrap();
    symlink(root.join("missing-cursor-rule"), &rule).unwrap();

    let preview = run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
    );
    assert!(preview.contains("Skip cursor Rule:codex-agents"));
    run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    );
    assert!(fs::symlink_metadata(rule).unwrap().file_type().is_symlink());
}

#[test]
fn portable_only_refuses_a_reused_pack_with_references() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("reused-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
        ],
    );
    let manifest_before = fs::read(pack.join("agent-sync.manifest.json")).unwrap();

    let error = run_failure(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    assert!(error.contains("export requires an empty pack"));
    assert!(pack.join("references/codex-memories/MEMORY.md").exists());
    assert_eq!(
        fs::read(pack.join("agent-sync.manifest.json")).unwrap(),
        manifest_before
    );
}

#[cfg(unix)]
#[test]
fn symlinked_shared_skills_export_as_real_directories() {
    let temp = setup_fixture();
    let root = temp.path();
    let real_skill = root.join("external-skills/symlinked-shared");
    write(
        &real_skill.join("SKILL.md"),
        r#"---
name: symlinked-shared
description: Symlinked shared skill
---

# Symlinked Shared
"#,
    );
    std::os::unix::fs::symlink(&real_skill, root.join(".agents/skills/symlinked-shared")).unwrap();

    let pack = root.join("pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
        ],
    );

    let packed_skill = pack.join("skills/symlinked-shared");
    assert!(packed_skill.join("SKILL.md").exists());
    assert!(!fs::symlink_metadata(&packed_skill)
        .unwrap()
        .file_type()
        .is_symlink());

    run(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
            "--yes",
        ],
    );

    let claude_skill = root.join(".claude/skills/symlinked-shared");
    assert!(claude_skill.join("SKILL.md").exists());
    assert!(!fs::symlink_metadata(&claude_skill)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn raw_mcp_headers_block_export() {
    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".claude.json"),
        r#"{
  "mcpServers": {
    "unsafe": {
      "type": "http",
      "url": "https://example.invalid/mcp",
      "headers": {
        "Authorization": "literal-value-that-should-not-export"
      }
    },
    "safe": {
      "type": "http",
      "url": "https://safe.example.invalid/mcp",
      "headers": {
        "Authorization": "${SAFE_AUTH_ENV}"
      }
    }
  }
}
"#,
    );
    let pack = root.join("pack");

    let error = run_failure(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "claude",
        ],
    );

    assert!(error.contains("MCP server `unsafe`"), "{error}");
    assert!(error.contains("unsupported value"), "{error}");
    assert!(!pack.join("mcp/servers.json").exists());
}

#[test]
fn export_can_limit_mcp_servers_to_a_reviewed_allowlist() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("allowlisted-pack");
    let mut codex_config = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    codex_config.push_str(
        r#"
[mcp_servers.unselected_secret]
command = "/usr/local/bin/unselected"
args = ["--api-key", "literal-secret-that-must-not-export"]
"#,
    );
    write(&root.join(".codex/config.toml"), &codex_config);

    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--mcp-servers",
            "qmd",
        ],
    );

    let exported: Value =
        serde_json::from_str(&fs::read_to_string(pack.join("mcp/servers.json")).unwrap()).unwrap();
    assert!(exported.get("qmd").is_some());
    assert!(exported.get("example_http").is_none());
    assert!(exported.get("unselected_secret").is_none());
    assert!(!fs::read_to_string(pack.join("mcp/servers.json"))
        .unwrap()
        .contains("literal-secret-that-must-not-export"));

    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(pack.join("agent-sync.manifest.json")).unwrap())
            .unwrap();
    let mcp_names = manifest["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|resource| resource["kind"] == "mcp")
        .filter_map(|resource| resource["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(mcp_names, vec!["qmd"]);
}

#[test]
fn unlisted_mcp_server_is_rejected_before_diff_verify_or_apply() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("unlisted-mcp-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );

    let mcp_path = pack.join("mcp/servers.json");
    let mut servers: Value = serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
    let hidden = servers["qmd"].clone();
    servers
        .as_object_mut()
        .unwrap()
        .insert("hidden".to_string(), hidden);
    let mcp_raw = [serde_json::to_vec_pretty(&servers).unwrap(), b"\n".to_vec()].concat();
    fs::write(&mcp_path, &mcp_raw).unwrap();

    let manifest_path = pack.join("agent-sync.manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let mcp_hash = format!("{:x}", Sha256::digest(&mcp_raw));
    for resource in manifest["resources"].as_array_mut().unwrap() {
        if resource["kind"] == "mcp" {
            resource["sha256"] = Value::String(mcp_hash.clone());
        }
    }
    fs::write(
        &manifest_path,
        [
            serde_json::to_vec_pretty(&manifest).unwrap(),
            b"\n".to_vec(),
        ]
        .concat(),
    )
    .unwrap();

    let cursor_before = fs::read(root.join(".cursor/mcp.json")).unwrap();
    for arguments in [
        vec![
            "diff",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
        vec![
            "verify",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
        ],
        vec![
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "cursor",
            "--yes",
        ],
    ] {
        let error = run_failure(root, &arguments);
        assert!(error.contains("MCP authorization mismatch"), "{error}");
        assert!(error.contains("unlisted server(s): hidden"), "{error}");
    }
    assert_eq!(
        fs::read(root.join(".cursor/mcp.json")).unwrap(),
        cursor_before
    );
}

#[test]
fn raw_mcp_credentials_in_args_urls_env_and_helpers_block_export() {
    let codex_cases = [
        (
            "args",
            r#"[mcp_servers.unsafe]
command = "/usr/local/bin/unsafe"
args = ["--api-key", "literal-secret-value"]
"#,
            "argument",
        ),
        (
            "url",
            r#"[mcp_servers.unsafe]
url = "https://user:literal-password@example.invalid/mcp"
"#,
            "URL",
        ),
        (
            "env",
            r#"[mcp_servers.unsafe]
command = "/usr/local/bin/unsafe"

[mcp_servers.unsafe.env]
SERVICE_PASSWORD = "literal-secret-value"
"#,
            "SERVICE_PASSWORD",
        ),
        (
            "inline-env",
            r#"[mcp_servers.unsafe]
command = "/usr/local/bin/unsafe"
env = { apiKey = "literal-secret-value" }
"#,
            "apiKey",
        ),
    ];
    for (case, config, expected) in codex_cases {
        let temp = setup_fixture();
        let root = temp.path();
        write(&root.join(".codex/config.toml"), config);
        let pack = root.join(format!("unsafe-{case}-pack"));

        let error = run_failure(
            root,
            &[
                "export",
                "--pack",
                pack.to_str().unwrap(),
                "--from",
                "codex",
                "--portable-only",
            ],
        );

        assert!(error.contains("MCP server `unsafe`"), "{case}: {error}");
        assert!(error.contains(expected), "{case}: {error}");
        assert!(!pack.join("mcp/servers.json").exists());
    }

    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".claude.json"),
        r#"{
  "mcpServers": {
    "unsafe": {
      "type": "http",
      "url": "https://example.invalid/mcp",
      "headersHelper": "/usr/local/bin/header-helper --token literal-secret-value"
    }
  }
}
"#,
    );
    let pack = root.join("unsafe-helper-pack");
    let error = run_failure(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "claude",
            "--portable-only",
        ],
    );
    assert!(error.contains("MCP server `unsafe`"), "{error}");
    assert!(error.contains("headersHelper"), "{error}");
    assert!(!pack.join("mcp/servers.json").exists());
}

#[test]
fn sensitive_files_and_raw_credentials_in_exported_trees_block_export() {
    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".codex/skills/leaky/.env"),
        "SERVICE_TOKEN=literal-secret-value\n",
    );
    write(
        &root.join(".codex/skills/leaky/SKILL.md"),
        "---\nname: leaky\ndescription: Leaky fixture\n---\n",
    );
    let pack = root.join("sensitive-skill-pack");

    let error = run_failure(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    assert!(error.contains("skill `leaky`"), "{error}");
    assert!(error.contains(".env"), "{error}");
    assert!(!pack.join("skills/leaky/.env").exists());

    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".codex/memories/reference/config.json"),
        "{\"apiKey\": \"literal-secret-value\"}\n",
    );
    let pack = root.join("sensitive-reference-pack");
    let error = run_failure(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
        ],
    );
    assert!(error.contains("Codex memory references"), "{error}");
    assert!(error.contains("config.json"), "{error}");
    assert!(!pack
        .join("references/codex-memories/reference/config.json")
        .exists());
}

#[test]
fn export_scanner_handles_source_types_passphrases_tokens_and_binary_credentials() {
    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".codex/skills/typed/SKILL.md"),
        "---\nname: typed\ndescription: Typed fixture\n---\n```rust\nstruct Config {\n    token: Option<String>,\n}\n```\nToken: authentication token used by the service.\n",
    );
    let safe_pack = root.join("typed-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            safe_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    assert!(safe_pack.join("skills/typed/SKILL.md").is_file());

    for (name, content) in [
        ("passphrase", "password: correct horse battery staple\n"),
        (
            "bare-env-shape",
            "API_TOKEN=PRODUCTION_SECRET_VALUE_12345\n",
        ),
        ("github-token", "ghp_12345678901234567890\n"),
    ] {
        let temp = setup_fixture();
        let root = temp.path();
        write(
            &root.join(format!(".codex/skills/{name}/SKILL.md")),
            &format!("---\nname: {name}\ndescription: Scanner fixture\n---\n{content}"),
        );
        let pack = root.join(format!("{name}-pack"));

        let error = run_failure(
            root,
            &[
                "export",
                "--pack",
                pack.to_str().unwrap(),
                "--from",
                "codex",
                "--portable-only",
            ],
        );

        assert!(
            error.contains("refusing to export skill"),
            "{name}: {error}"
        );
        assert!(!pack.join(format!("skills/{name}/SKILL.md")).exists());
    }

    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".codex/skills/binary/SKILL.md"),
        "---\nname: binary\ndescription: Binary fixture\n---\n",
    );
    fs::write(
        root.join(".codex/skills/binary/credential-store.bin"),
        [0xff, 0x00, 0x01],
    )
    .unwrap();
    let pack = root.join("binary-pack");

    let error = run_failure(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );

    assert!(error.contains("binary credential container"), "{error}");
    assert!(!pack.join("skills/binary/credential-store.bin").exists());
}

#[test]
fn init_and_status_are_safe_read_only_entrypoints() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("empty-pack");

    let init = run(root, &["init", "--pack", pack.to_str().unwrap()]);
    assert!(init.contains("Initialized agent-sync pack"));
    assert!(pack.join("agent-sync.manifest.json").exists());
    assert!(pack.join("skills").is_dir());
    assert!(pack.join("rules").is_dir());

    let status_without_pack = run(root, &["status"]);
    assert!(status_without_pack.contains("Codex"));
    assert!(status_without_pack.contains("Claude"));

    let status_with_pack = run(
        root,
        &[
            "status",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
        ],
    );
    assert_eq!(status_with_pack, "No changes.\n");
}

#[test]
fn managed_setup_sync_status_and_automation_are_one_command_workflows() {
    let temp = setup_fixture();
    let root = temp.path();

    let setup_preview = run(
        root,
        &[
            "setup",
            "--from",
            "codex",
            "--to",
            "cursor",
            "--mcp-servers",
            "qmd",
        ],
    );
    assert!(setup_preview.contains("Dry run"));
    assert!(!root.join(".agent-sync/config.toml").exists());
    assert!(!root.join(".agent-sync/state").exists());
    assert!(!root.join(".agents/skills/agent-sync/SKILL.md").exists());

    let setup = run(
        root,
        &[
            "setup",
            "--from",
            "codex",
            "--to",
            "cursor",
            "--mcp-servers",
            "qmd",
            "--yes",
        ],
    );
    assert!(setup.contains("Saved managed config"));
    assert!(setup.contains("Installed bundled agent-sync skill"));
    assert!(root.join(".agent-sync/config.toml").exists());
    assert!(root.join(".agents/skills/agent-sync/SKILL.md").exists());

    let preview = run(root, &["sync"]);
    assert!(preview.contains("Preview"));
    assert!(!root.join(".agent-sync/state/runs.jsonl").exists());
    assert!(!root
        .join(".cursor/rules/imported-codex-agents.mdc")
        .exists());
    let cursor_before: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert!(cursor_before["mcpServers"].get("qmd").is_none());

    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("Verification passed"));
    assert!(root
        .join(".cursor/rules/imported-codex-agents.mdc")
        .exists());
    let cursor_after: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert_eq!(
        cursor_after["mcpServers"]["qmd"]["command"],
        "/usr/local/bin/qmd"
    );
    assert!(root.join(".agent-sync/state/last-attempt.json").exists());
    assert!(root.join(".agent-sync/state/last-success.json").exists());
    assert!(root.join(".agent-sync/state/runs.jsonl").exists());

    let status = run(root, &["status"]);
    assert!(status.contains("Managed route: codex -> cursor"));
    assert!(status.contains("Cursor history: disabled"));
    assert!(status.contains("Health: healthy"), "{status}");
    assert!(status.contains("Drift: 0 add, 0 update"));

    let automated = run(root, &["sync", "--yes", "--automation"]);
    assert_eq!(automated, "DONT_NOTIFY\n");
}

#[test]
fn doctor_rejects_missing_run_history_after_a_successful_sync() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);
    fs::remove_file(root.join(".agent-sync/state/runs.jsonl")).unwrap();

    let doctor = run_output(root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);

    assert!(!doctor.status.success());
    assert!(
        stdout.contains("run history state: run history file is missing"),
        "{stdout}"
    );
}

#[test]
fn doctor_rejects_missing_last_attempt_after_a_successful_sync() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);
    fs::remove_file(root.join(".agent-sync/state/last-attempt.json")).unwrap();

    let doctor = run_output(root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);

    assert!(!doctor.status.success());
    assert!(
        stdout.contains("last attempt state: last attempt file is missing"),
        "{stdout}"
    );
}

#[test]
fn doctor_rejects_missing_last_success_after_a_successful_sync() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);
    fs::remove_file(root.join(".agent-sync/state/last-success.json")).unwrap();

    let doctor = run_output(root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);

    assert!(!doctor.status.success());
    assert!(
        stdout.contains("last success state: last success file is missing"),
        "{stdout}"
    );
}

#[test]
fn doctor_uses_attempt_order_instead_of_wall_clock_for_failures() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);

    let attempt_path = root.join(".agent-sync/state/last-attempt.json");
    let history_path = root.join(".agent-sync/state/runs.jsonl");
    let mut attempt: Value =
        serde_json::from_str(&fs::read_to_string(&attempt_path).unwrap()).unwrap();
    attempt["run_id"] = Value::String("clock-rollback-failure".to_string());
    attempt["result"] = Value::String("failed".to_string());
    attempt["finished_at"] = Value::String("2000-01-01T00:00:00Z".to_string());
    attempt["error"] = Value::String("simulated failure after clock rollback".to_string());
    let compact = serde_json::to_string(&attempt).unwrap();
    fs::write(
        &attempt_path,
        [serde_json::to_vec_pretty(&attempt).unwrap(), b"\n".to_vec()].concat(),
    )
    .unwrap();
    let mut history = OpenOptions::new().append(true).open(history_path).unwrap();
    writeln!(history, "{compact}").unwrap();

    let doctor = run_output(root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(!doctor.status.success());
    assert!(stdout.contains("latest sync attempt failed"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn doctor_rejects_unwritable_run_state() {
    use std::os::unix::fs::PermissionsExt;

    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);
    let state = root.join(".agent-sync/state");
    let runs = state.join("runs.jsonl");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o444)).unwrap();

    let doctor = run_output(root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);

    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!doctor.status.success());
    assert!(stdout.contains("run state directory"), "{stdout}");
    assert!(stdout.contains("run history persistence"), "{stdout}");
}

#[test]
fn managed_sync_preserves_a_cursor_owned_target_created_after_preview() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);

    let preview = run(root, &["sync"]);
    assert!(preview.contains("Add cursor Rule:codex-agents"));

    let cursor_rule = root.join(".cursor/rules/imported-codex-agents.mdc");
    let cursor_owned = "Cursor-owned rule created after preview\n";
    write(&cursor_rule, cursor_owned);

    let applied = run(root, &["sync", "--yes"]);

    assert!(applied.contains("Preserved target-owned resources"));
    assert_eq!(fs::read_to_string(cursor_rule).unwrap(), cursor_owned);
}

#[test]
fn managed_setup_does_not_sync_any_mcp_server_by_default() {
    let temp = setup_fixture();
    let root = temp.path();
    let original = fs::read_to_string(root.join(".cursor/mcp.json")).unwrap();

    run(root, &["setup", "--yes"]);
    let config = fs::read_to_string(root.join(".agent-sync/config.toml")).unwrap();
    assert!(config.contains("mode = \"none\""));
    run(root, &["sync", "--yes"]);

    assert_eq!(
        fs::read_to_string(root.join(".cursor/mcp.json")).unwrap(),
        original
    );
}

#[test]
fn managed_setup_does_not_adopt_an_identical_user_owned_skill() {
    let temp = setup_fixture();
    let root = temp.path();
    let user_skill = include_str!("../skills/agent-sync/SKILL.md");
    write(&root.join(".agents/skills/agent-sync/SKILL.md"), user_skill);

    let error = run_failure(root, &["setup", "--yes"]);

    assert!(error.contains("unmanaged agent-sync skill"));
    assert_eq!(
        fs::read_to_string(root.join(".agents/skills/agent-sync/SKILL.md")).unwrap(),
        user_skill
    );
    assert!(!root.join(".agent-sync/config.toml").exists());
}

#[test]
fn managed_sync_ignores_unrelated_malformed_mcp_configs_when_mcp_is_disabled() {
    let temp = setup_fixture();
    let root = temp.path();
    write(&root.join(".claude.json"), "not-json\n");
    write(&root.join(".cursor/mcp.json"), "also-not-json\n");

    run(root, &["setup", "--no-mcp", "--yes"]);
    run(root, &["sync", "--yes"]);

    assert_eq!(
        fs::read_to_string(root.join(".cursor/mcp.json")).unwrap(),
        "also-not-json\n"
    );
    assert!(root
        .join(".cursor/rules/imported-codex-agents.mdc")
        .exists());
}

#[test]
fn managed_sync_previews_cursor_history_hook_maintenance() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--cursor-history", "--skip-qmd", "--yes"]);

    let preview = run(root, &["sync"]);

    assert!(preview.contains("reconcile the managed Cursor history hook"));
    assert!(preview.contains("agent-sync sync --yes"));
    assert!(!root.join(".cursor/hooks.json").exists());
}

#[test]
fn managed_sync_maintains_its_owned_natural_language_skill() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    let skill_path = root.join(".agents/skills/agent-sync/SKILL.md");
    let state_path = root.join(".agent-sync/state/bundled-skill.json");
    let bundled = fs::read(&skill_path).unwrap();
    let old_managed = b"old managed agent-sync skill\n";
    fs::write(&skill_path, old_managed).unwrap();
    let mut state: Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    state["installed_sha256"] = Value::String(format!("{:x}", Sha256::digest(old_managed)));
    fs::write(
        &state_path,
        [serde_json::to_vec_pretty(&state).unwrap(), b"\n".to_vec()].concat(),
    )
    .unwrap();

    let preview = run(root, &["sync"]);
    assert!(preview.contains("refresh the natural-language agent-sync skill"));
    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("natural-language agent-sync skill was maintained"));
    assert_eq!(fs::read(&skill_path).unwrap(), bundled);
}

#[cfg(unix)]
#[test]
fn managed_setup_does_not_rewrite_an_unchanged_config() {
    use std::os::unix::fs::MetadataExt;

    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    let config = root.join(".agent-sync/config.toml");
    let inode = fs::metadata(&config).unwrap().ino();

    run(root, &["setup", "--yes"]);

    assert_eq!(fs::metadata(config).unwrap().ino(), inode);
}

#[test]
fn managed_sync_preserves_claude_conflicts_and_adds_missing_resources() {
    let temp = setup_fixture();
    let root = temp.path();
    let claude_skill = "---\nname: pr-review\ndescription: Claude-owned\n---\n";
    write(
        &root.join(".claude/skills/pr-review/SKILL.md"),
        claude_skill,
    );
    let claude_qmd = serde_json::json!({
        "type": "stdio",
        "command": "/opt/claude-qmd",
        "args": ["mcp"],
        "env": {},
        "claudeOnly": true
    });
    write(
        &root.join(".claude.json"),
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {"qmd": claude_qmd.clone()},
                "claudeSpecific": true
            }))
            .unwrap()
        ),
    );

    run(
        root,
        &[
            "setup",
            "--from",
            "codex",
            "--to",
            "claude",
            "--mcp-servers",
            "qmd,example_http",
            "--yes",
        ],
    );
    let preview = run(root, &["sync"]);
    assert!(
        preview.contains("Preserved target-owned resources:"),
        "{preview}"
    );
    assert!(preview.contains("- claude Skill:pr-review"), "{preview}");
    assert!(
        preview.contains("Add claude Skill:shared-style"),
        "{preview}"
    );
    assert!(
        preview.contains("Add claude Rule:codex-agents"),
        "{preview}"
    );
    assert!(preview.contains("- claude Mcp:qmd"), "{preview}");
    assert!(preview.contains("Add claude Mcp:example_http"), "{preview}");
    assert!(!preview.contains("This plan is blocked"), "{preview}");

    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("Verification passed"), "{applied}");
    assert_eq!(
        fs::read_to_string(root.join(".claude/skills/pr-review/SKILL.md")).unwrap(),
        claude_skill
    );
    assert!(root.join(".claude/skills/shared-style/SKILL.md").is_file());
    assert!(root
        .join(".claude/rules/imported-codex-agents.md")
        .is_file());
    let claude: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(claude["mcpServers"]["qmd"], claude_qmd);
    assert_eq!(claude["claudeSpecific"], true);
    assert_eq!(
        claude["mcpServers"]["example_http"]["url"],
        "https://mcp.example.invalid/mcp"
    );
    assert!(root.join(".agent-sync/state/last-success.json").is_file());

    let status: Value = serde_json::from_str(&run(root, &["status", "--format", "json"])).unwrap();
    assert_eq!(status["healthy"], true, "{status:#}");
    assert_eq!(status["drift"]["add"], 0);
    assert_eq!(status["drift"]["update"], 0);
    assert!(status["drift"]["preserved"].as_u64().unwrap() >= 2);
    assert_eq!(status["next_action"], "none");

    let doctor = run_output(root, &["doctor"]);
    assert!(
        doctor.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(
        String::from_utf8_lossy(&doctor.stdout).contains("intentionally preserved"),
        "{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
    assert_eq!(
        run(root, &["sync", "--yes", "--automation"]),
        "DONT_NOTIFY\n"
    );
}

#[test]
fn managed_sync_reuses_existing_claude_codex_rule_destination() {
    let temp = setup_fixture();
    let root = temp.path();
    let legacy_rule = "# Claude-specific Codex guidance\n\nKeep this rule.\n";
    let legacy_path = root.join(".claude/rules/codex-global-agent-rules.md");
    write(&legacy_path, legacy_rule);

    run(
        root,
        &["setup", "--from", "codex", "--to", "claude", "--yes"],
    );
    let preview = run(root, &["sync"]);
    assert!(preview.contains("- claude Rule:codex-agents"), "{preview}");

    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("Verification passed"), "{applied}");
    assert_eq!(fs::read_to_string(&legacy_path).unwrap(), legacy_rule);
    assert!(!root.join(".claude/rules/imported-codex-agents.md").exists());
}

#[test]
fn moved_managed_claude_rule_is_preserved_without_blocking_additions() {
    let temp = setup_fixture();
    let root = temp.path();

    run(
        root,
        &["setup", "--from", "codex", "--to", "claude", "--yes"],
    );
    run(root, &["sync", "--yes"]);
    let imported = root.join(".claude/rules/imported-codex-agents.md");
    let legacy = root.join(".claude/rules/codex-global-agent-rules.md");
    let original_rule = fs::read_to_string(&imported).unwrap();
    fs::rename(&imported, &legacy).unwrap();
    write(
        &root.join(".codex/AGENTS.md"),
        "# Global Agent Rules\n\n- Changed after the move.\n",
    );
    write(
        &root.join(".codex/skills/late-skill/SKILL.md"),
        "---\nname: late-skill\ndescription: Added later\n---\n\n# Late Skill\n",
    );

    let preview = run(root, &["sync"]);
    assert!(preview.contains("- claude Rule:codex-agents"), "{preview}");
    assert!(preview.contains("Add claude Skill:late-skill"), "{preview}");

    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("Verification passed"), "{applied}");
    assert_eq!(fs::read_to_string(&legacy).unwrap(), original_rule);
    assert!(!imported.exists());
    assert!(root.join(".claude/skills/late-skill/SKILL.md").is_file());
}

#[cfg(unix)]
#[test]
fn managed_sync_preserves_a_symlinked_claude_skill() {
    use std::os::unix::fs::symlink;

    let temp = setup_fixture();
    let root = temp.path();
    let victim = root.join("claude-owned-pr-review");
    let victim_skill =
        "---\nname: pr-review\ndescription: Claude-owned\n---\n\n# Keep this skill\n";
    write(&victim.join("SKILL.md"), victim_skill);
    let link = root.join(".claude/skills/pr-review");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&victim, &link).unwrap();

    run(
        root,
        &["setup", "--from", "codex", "--to", "claude", "--yes"],
    );
    let preview = run(root, &["sync"]);
    assert!(preview.contains("- claude Skill:pr-review"), "{preview}");
    assert!(
        preview.contains("Add claude Skill:shared-style"),
        "{preview}"
    );

    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("Verification passed"), "{applied}");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(victim.join("SKILL.md")).unwrap(),
        victim_skill
    );
    assert!(root.join(".claude/skills/shared-style/SKILL.md").is_file());
}

#[test]
fn managed_claude_resources_update_from_recorded_state() {
    let temp = setup_fixture();
    let root = temp.path();

    run(
        root,
        &[
            "setup",
            "--from",
            "codex",
            "--to",
            "claude",
            "--mcp-servers",
            "qmd",
            "--yes",
        ],
    );
    run(root, &["sync", "--yes"]);

    write(
        &root.join(".codex/skills/pr-review/SKILL.md"),
        "---\nname: pr-review\ndescription: Review PRs\n---\n\n# PR Review v2\n",
    );
    write(
        &root.join(".codex/AGENTS.md"),
        "# Global Agent Rules\n\n- Keep changes scoped.\n- Use focused tests.\n",
    );
    write(
        &root.join(".codex/config.toml"),
        "[mcp_servers.qmd]\ncommand = \"/opt/qmd-v2\"\nargs = [\"mcp\", \"--new\"]\n",
    );

    let preview = run(root, &["sync"]);
    assert!(
        preview.contains("ManagedUpdate claude Skill:pr-review"),
        "{preview}"
    );
    assert!(
        preview.contains("ManagedUpdate claude Rule:codex-agents"),
        "{preview}"
    );
    assert!(
        preview.contains("ManagedUpdate claude Mcp:qmd"),
        "{preview}"
    );
    assert!(!preview.contains("This plan is blocked"), "{preview}");

    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("Verification passed"), "{applied}");
    assert!(
        fs::read_to_string(root.join(".claude/skills/pr-review/SKILL.md"))
            .unwrap()
            .contains("# PR Review v2")
    );
    assert!(
        fs::read_to_string(root.join(".claude/rules/imported-codex-agents.md"))
            .unwrap()
            .contains("Use focused tests.")
    );
    let claude: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(claude["mcpServers"]["qmd"]["command"], "/opt/qmd-v2");
    assert_eq!(
        claude["mcpServers"]["qmd"]["args"],
        serde_json::json!(["mcp", "--new"])
    );
}

#[test]
fn manual_claude_edits_are_preserved_without_starving_new_adds() {
    let temp = setup_fixture();
    let root = temp.path();

    run(
        root,
        &[
            "setup",
            "--from",
            "codex",
            "--to",
            "claude",
            "--mcp-servers",
            "qmd",
            "--yes",
        ],
    );
    run(root, &["sync", "--yes"]);

    let edited_skill = "---\nname: pr-review\ndescription: Claude edit\n---\n\n# Keep this edit\n";
    write(
        &root.join(".claude/skills/pr-review/SKILL.md"),
        edited_skill,
    );
    let edited_rule = "# Claude-specific imported rules\n\nKeep this edit.\n";
    write(
        &root.join(".claude/rules/imported-codex-agents.md"),
        edited_rule,
    );
    let claude_path = root.join(".claude.json");
    let mut claude: Value =
        serde_json::from_str(&fs::read_to_string(&claude_path).unwrap()).unwrap();
    claude["mcpServers"]["qmd"]["command"] = serde_json::json!("/opt/claude-qmd");
    let edited_qmd = claude["mcpServers"]["qmd"].clone();
    write(
        &claude_path,
        &format!("{}\n", serde_json::to_string_pretty(&claude).unwrap()),
    );

    write(
        &root.join(".codex/skills/late-skill/SKILL.md"),
        "---\nname: late-skill\ndescription: Added later\n---\n\n# Late Skill\n",
    );
    write(
        &root.join(".codex/config.toml"),
        "[mcp_servers.qmd]\ncommand = \"/usr/local/bin/qmd\"\nargs = [\"mcp\"]\n\n[mcp_servers.late_mcp]\ncommand = \"/opt/late-mcp\"\nargs = []\n",
    );
    run(root, &["setup", "--mcp-servers", "qmd,late_mcp", "--yes"]);

    let preview = run(root, &["sync"]);
    assert!(
        preview.contains("Preserved target-owned resources:"),
        "{preview}"
    );
    assert!(preview.contains("- claude Skill:pr-review"), "{preview}");
    assert!(preview.contains("- claude Rule:codex-agents"), "{preview}");
    assert!(preview.contains("- claude Mcp:qmd"), "{preview}");
    assert!(preview.contains("Add claude Skill:late-skill"), "{preview}");
    assert!(preview.contains("Add claude Mcp:late_mcp"), "{preview}");

    let applied = run(root, &["sync", "--yes"]);
    assert!(applied.contains("Verification passed"), "{applied}");
    assert_eq!(
        fs::read_to_string(root.join(".claude/skills/pr-review/SKILL.md")).unwrap(),
        edited_skill
    );
    assert_eq!(
        fs::read_to_string(root.join(".claude/rules/imported-codex-agents.md")).unwrap(),
        edited_rule
    );
    assert!(root.join(".claude/skills/late-skill/SKILL.md").is_file());
    let claude: Value = serde_json::from_str(&fs::read_to_string(claude_path).unwrap()).unwrap();
    assert_eq!(claude["mcpServers"]["qmd"], edited_qmd);
    assert_eq!(claude["mcpServers"]["late_mcp"]["command"], "/opt/late-mcp");
}

#[test]
fn preexisting_identical_claude_resources_are_not_adopted() {
    let temp = setup_fixture();
    let root = temp.path();
    let original_skill = fs::read_to_string(root.join(".codex/skills/pr-review/SKILL.md")).unwrap();
    let original_shared =
        fs::read_to_string(root.join(".agents/skills/shared-style/SKILL.md")).unwrap();
    write(
        &root.join(".claude/skills/pr-review/SKILL.md"),
        &original_skill,
    );
    write(
        &root.join(".claude/skills/shared-style/SKILL.md"),
        &original_shared,
    );
    let original_source_rule = fs::read_to_string(root.join(".codex/AGENTS.md")).unwrap();
    let original_rule = format!(
        "# Imported Codex Agent Rules\n\nImported by `agent-sync` from pack resource `codex-agents`.\n\n{original_source_rule}"
    );
    write(
        &root.join(".claude/rules/imported-codex-agents.md"),
        &original_rule,
    );
    let original_qmd = serde_json::json!({
        "type": "stdio",
        "command": "/usr/local/bin/qmd",
        "args": ["mcp"],
        "env": {}
    });
    write(
        &root.join(".claude.json"),
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {"qmd": original_qmd.clone()}
            }))
            .unwrap()
        ),
    );

    run(
        root,
        &[
            "setup",
            "--from",
            "codex",
            "--to",
            "claude",
            "--mcp-servers",
            "qmd",
            "--yes",
        ],
    );
    let initial_pack = root.join("preexisting-identical-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
            "--mcp-servers",
            "qmd",
        ],
    );
    let first_preview = run(
        root,
        &[
            "diff",
            "--pack",
            initial_pack.to_str().unwrap(),
            "--targets",
            "claude",
        ],
    );
    assert!(
        first_preview.contains("Unchanged claude Skill:pr-review"),
        "{first_preview}"
    );
    assert!(
        first_preview.contains("Unchanged claude Rule:codex-agents"),
        "{first_preview}"
    );
    assert!(
        first_preview.contains("Unchanged claude Mcp:qmd"),
        "{first_preview}"
    );
    run(root, &["sync", "--yes"]);
    assert!(!root
        .join(".agent-sync/state/claude-resources.json")
        .exists());
    assert!(!root.join(".agent-sync/state/claude-mcp.json").exists());

    write(
        &root.join(".codex/skills/pr-review/SKILL.md"),
        "---\nname: pr-review\ndescription: Review PRs\n---\n\n# Changed source\n",
    );
    write(
        &root.join(".codex/AGENTS.md"),
        "# Global Agent Rules\n\n- Changed source.\n",
    );
    write(
        &root.join(".codex/config.toml"),
        "[mcp_servers.qmd]\ncommand = \"/opt/qmd-v2\"\nargs = [\"mcp\"]\n",
    );

    let changed_preview = run(root, &["sync"]);
    assert!(
        changed_preview.contains("- claude Skill:pr-review"),
        "{changed_preview}"
    );
    assert!(
        changed_preview.contains("- claude Rule:codex-agents"),
        "{changed_preview}"
    );
    assert!(
        changed_preview.contains("- claude Mcp:qmd"),
        "{changed_preview}"
    );
    run(root, &["sync", "--yes"]);

    assert_eq!(
        fs::read_to_string(root.join(".claude/skills/pr-review/SKILL.md")).unwrap(),
        original_skill
    );
    assert_eq!(
        fs::read_to_string(root.join(".claude/rules/imported-codex-agents.md")).unwrap(),
        original_rule
    );
    let claude: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(claude["mcpServers"]["qmd"], original_qmd);
}

#[test]
fn setup_patches_existing_policy_instead_of_resetting_unspecified_fields() {
    let temp = setup_fixture();
    let root = temp.path();

    run(
        root,
        &[
            "setup",
            "--from",
            "codex",
            "--to",
            "cursor",
            "--mcp-servers",
            "qmd",
            "--yes",
        ],
    );
    let preview = run(root, &["setup", "--cursor-history"]);
    assert!(preview.contains("MCP: selected (qmd)"));
    assert!(preview.contains("Cursor history: enabled without QMD refresh"));
    run(root, &["setup", "--cursor-history", "--yes"]);

    let config = fs::read_to_string(root.join(".agent-sync/config.toml")).unwrap();
    assert!(config.contains("mode = \"selected\""));
    assert!(config.contains("servers = [\"qmd\"]"));
    assert!(config.contains("enabled = true"));
    assert!(config.contains("refresh_qmd = false"));
}

#[test]
fn managed_sync_removes_only_its_history_hook_when_history_is_disabled() {
    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".cursor/hooks.json"),
        r#"{"version":1,"hooks":{"stop":[{"command":"custom-hook","timeout":12}]}}"#,
    );

    run(root, &["setup", "--cursor-history", "--skip-qmd", "--yes"]);
    run(root, &["sync", "--yes"]);
    let installed: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".cursor/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(installed["hooks"]["stop"][0]["command"], "custom-hook");
    let managed_command = installed["hooks"]["stop"][1]["command"].as_str().unwrap();
    assert!(managed_command.contains(" cursor-history export --output-dir "));
    assert!(managed_command.ends_with("--skip-qmd # agent-sync-managed-hook-v1"));

    run(root, &["setup", "--no-cursor-history", "--yes"]);
    run(root, &["sync", "--yes"]);
    let removed: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".cursor/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(removed["hooks"]["stop"].as_array().unwrap().len(), 1);
    assert_eq!(removed["hooks"]["stop"][0]["command"], "custom-hook");
}

#[test]
fn status_exposes_when_cursor_history_skips_qmd() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--cursor-history", "--skip-qmd", "--yes"]);
    run(root, &["sync", "--yes"]);

    let status = run(root, &["status"]);
    assert!(status.contains("Cursor history: enabled without QMD refresh"));

    let repair = run(root, &["setup", "--refresh-qmd"]);
    assert!(repair.contains("Cursor history: enabled with QMD refresh"));
}

#[test]
fn malformed_cursor_transcript_does_not_block_sync_and_remains_unhealthy() {
    let temp = setup_fixture();
    let root = temp.path();
    let transcript =
        root.join(".cursor/projects/example/agent-transcripts/future-format/future-format.jsonl");
    write(&transcript, "{\"version\":999,\"newCursorSchema\":true}\n");
    run(root, &["setup", "--cursor-history", "--skip-qmd", "--yes"]);

    let sync = run(root, &["sync", "--yes"]);
    assert!(sync.contains("Skipped 1 unreadable Cursor transcript"));
    assert!(root
        .join(".cursor/rules/imported-codex-agents.mdc")
        .exists());

    let status: Value = serde_json::from_str(&run(root, &["status", "--format", "json"])).unwrap();
    assert_eq!(status["healthy"], false);
    assert!(status["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error
            .as_str()
            .is_some_and(|error| error.contains("1 unreadable transcript"))));

    let doctor = run_output(root, &["doctor"]);
    assert!(!doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("1 unreadable"));
}

#[test]
fn doctor_rejects_an_invalid_cursor_hook_version() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--cursor-history", "--skip-qmd", "--yes"]);
    run(root, &["sync", "--yes"]);
    let hooks_path = root.join(".cursor/hooks.json");
    let mut hooks: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    hooks["version"] = Value::Null;
    fs::write(
        &hooks_path,
        [serde_json::to_vec_pretty(&hooks).unwrap(), b"\n".to_vec()].concat(),
    )
    .unwrap();

    let doctor = run_output(root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);

    assert!(!doctor.status.success());
    assert!(stdout.contains("numeric value 1"), "{stdout}");
    let unchanged: Value = serde_json::from_str(&fs::read_to_string(hooks_path).unwrap()).unwrap();
    assert_eq!(unchanged["version"], Value::Null);
}

#[cfg(unix)]
#[test]
fn doctor_and_sync_reject_a_dangling_sync_lock_symlink() {
    use std::os::unix::fs::symlink;

    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);
    let lock = root.join(".agent-sync/state/sync.lock");
    symlink(root.join("missing-lock-target"), &lock).unwrap();

    let doctor = run_output(root, &["doctor"]);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(!doctor.status.success());
    assert!(
        stdout.contains("agent sync lock path is a symlink"),
        "{stdout}"
    );

    let error = run_failure(root, &["sync", "--yes"]);
    assert!(error.contains("sync lock path is a symlink"), "{error}");
    assert!(fs::symlink_metadata(lock).unwrap().file_type().is_symlink());
}

#[test]
fn manifest_hash_tampering_is_rejected_before_diff_or_apply() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("tampered-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    write(
        &pack.join("skills/pr-review/SKILL.md"),
        "tampered after export\n",
    );

    let error = run_failure(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
            "--yes",
        ],
    );
    assert!(error.contains("failed hash validation"));
    assert!(!root.join(".claude/skills/pr-review").exists());
}

#[test]
fn unsafe_manifest_resource_names_are_rejected_before_apply() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("unsafe-name-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    let manifest_path = pack.join("agent-sync.manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let skill = manifest["resources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|resource| resource["kind"] == "skill")
        .unwrap();
    skill["name"] = Value::String("../../escaped".to_string());
    fs::write(
        &manifest_path,
        [
            serde_json::to_vec_pretty(&manifest).unwrap(),
            b"\n".to_vec(),
        ]
        .concat(),
    )
    .unwrap();

    let error = run_failure(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
            "--yes",
        ],
    );
    assert!(error.contains("unsafe name"));
    assert!(!root.join("escaped").exists());
}

#[test]
fn apply_rolls_back_completed_writes_when_a_later_resource_fails() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("rollback-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    let original_skill = "---\nname: pr-review\ndescription: Original Claude skill\n---\n";
    write(
        &root.join(".claude/skills/pr-review/SKILL.md"),
        original_skill,
    );
    write(
        &root.join(".claude/skills/pr-review/user-note.txt"),
        "keep this user file\n",
    );
    let invalid_mcp_state = root.join(".agent-sync/state/claude-mcp.json");
    write(&invalid_mcp_state, "not valid JSON\n");

    let error = run_failure(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
            "--yes",
        ],
    );

    assert!(error.contains("completed writes were rolled back"));
    assert_eq!(
        fs::read_to_string(root.join(".claude/skills/pr-review/SKILL.md")).unwrap(),
        original_skill
    );
    assert_eq!(
        fs::read_to_string(root.join(".claude/skills/pr-review/user-note.txt")).unwrap(),
        "keep this user file\n"
    );
    assert!(!root.join(".claude/skills/shared-style").exists());
    assert!(!root.join(".claude/rules/imported-codex-agents.md").exists());
    assert_eq!(
        fs::read_to_string(invalid_mcp_state).unwrap(),
        "not valid JSON\n"
    );
}

#[cfg(unix)]
#[test]
fn apply_rollback_restores_a_file_replaced_by_a_skill_directory() {
    use std::os::unix::fs::symlink;

    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("rollback-file-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    let original = root.join(".claude/skills/pr-review");
    write(&original, "original non-directory skill placeholder\n");
    let user_skill = root.join("user-owned-shared-style-file-rollback");
    write(
        &user_skill.join("SKILL.md"),
        "---\nname: shared-style\ndescription: User owned\n---\n",
    );
    let linked_skill = root.join(".claude/skills/shared-style");
    symlink(&user_skill, &linked_skill).unwrap();

    let error = run_failure(
        root,
        &[
            "apply",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
            "--yes",
        ],
    );

    assert!(
        error.contains("completed writes were rolled back"),
        "{error}"
    );
    assert!(original.is_file());
    assert_eq!(
        fs::read_to_string(original).unwrap(),
        "original non-directory skill placeholder\n"
    );
    assert!(fs::symlink_metadata(linked_skill)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn duplicate_manifest_resource_identities_are_rejected() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("duplicate-resource-pack");
    run(
        root,
        &[
            "export",
            "--pack",
            pack.to_str().unwrap(),
            "--from",
            "codex",
            "--portable-only",
        ],
    );
    let manifest_path = pack.join("agent-sync.manifest.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let duplicate = manifest["resources"].as_array().unwrap()[0].clone();
    manifest["resources"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    fs::write(
        &manifest_path,
        [
            serde_json::to_vec_pretty(&manifest).unwrap(),
            b"\n".to_vec(),
        ]
        .concat(),
    )
    .unwrap();

    let error = run_failure(
        root,
        &[
            "diff",
            "--pack",
            pack.to_str().unwrap(),
            "--targets",
            "claude",
        ],
    );
    assert!(error.contains("duplicate"));
}

#[cfg(unix)]
#[test]
fn managed_sync_rejects_a_symlinked_run_history_before_target_writes() {
    use std::os::unix::fs::symlink;

    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    let victim = root.join("victim-runs.jsonl");
    fs::write(&victim, "keep me\n").unwrap();
    symlink(&victim, root.join(".agent-sync/state/runs.jsonl")).unwrap();

    let error = run_failure(root, &["sync", "--yes"]);

    assert!(error.contains("symlinked run history"));
    assert_eq!(fs::read_to_string(victim).unwrap(), "keep me\n");
    assert!(!root
        .join(".cursor/rules/imported-codex-agents.mdc")
        .exists());
}

#[test]
fn cursor_can_be_the_managed_source_without_changing_cursor_owned_settings() {
    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".cursor/skills/cursor-only/SKILL.md"),
        "---\nname: cursor-only\ndescription: Cursor source skill\n---\n",
    );
    write(
        &root.join(".cursor/rules/personal.mdc"),
        "---\nalwaysApply: true\n---\nKeep this Cursor rule.\n",
    );
    write(
        &root.join(".cursor/settings.json"),
        "{\"cursorSpecific\":true}\n",
    );
    write(
        &root.join(".cursor/mcp.json"),
        "{\"mcpServers\":{\"cursor-tools\":{\"command\":\"cursor-tools\",\"args\":[\"serve\"]}},\"cursorOnly\":true}\n",
    );
    let original_mcp = fs::read_to_string(root.join(".cursor/mcp.json")).unwrap();
    let original_rule = fs::read_to_string(root.join(".cursor/rules/personal.mdc")).unwrap();
    let original_settings = fs::read_to_string(root.join(".cursor/settings.json")).unwrap();

    run(
        root,
        &[
            "setup",
            "--from",
            "cursor",
            "--to",
            "codex,claude",
            "--mcp-servers",
            "cursor-tools",
            "--yes",
        ],
    );
    let applied = run(root, &["sync", "--yes"]);

    assert!(applied.contains("cursor-only"));
    assert!(root.join(".codex/skills/cursor-only/SKILL.md").is_file());
    assert!(root.join(".claude/skills/cursor-only/SKILL.md").is_file());
    assert_eq!(
        fs::read_to_string(root.join(".cursor/mcp.json")).unwrap(),
        original_mcp
    );
    assert_eq!(
        fs::read_to_string(root.join(".cursor/rules/personal.mdc")).unwrap(),
        original_rule
    );
    assert_eq!(
        fs::read_to_string(root.join(".cursor/settings.json")).unwrap(),
        original_settings
    );

    let codex = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    assert!(codex.contains("[mcp_servers.cursor-tools]"));
    let claude: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(
        claude["mcpServers"]["cursor-tools"]["command"],
        "cursor-tools"
    );
}

#[test]
fn setup_installs_natural_language_control_for_claude() {
    let temp = setup_fixture();
    let root = temp.path();

    run(root, &["setup", "--yes"]);

    let shared = fs::read_to_string(root.join(".agents/skills/agent-sync/SKILL.md")).unwrap();
    let claude = fs::read_to_string(root.join(".claude/skills/agent-sync/SKILL.md")).unwrap();
    assert_eq!(claude, shared);
    assert!(root
        .join(".agent-sync/state/bundled-skill-claude.json")
        .is_file());
}

#[test]
fn managed_status_json_has_stable_health_and_background_fields() {
    let temp = setup_fixture();
    let root = temp.path();
    run(root, &["setup", "--yes"]);
    run(root, &["sync", "--yes"]);

    let status: Value = serde_json::from_str(&run(root, &["status", "--format", "json"])).unwrap();

    assert_eq!(status["source"], "codex");
    assert_eq!(status["configured"], true);
    assert_eq!(status["cursor_history"], "disabled");
    assert_eq!(status["healthy"], true, "{status:#}");
    assert!(status["drift"].is_object());
    assert!(status["background"]["supported"].is_boolean());
    assert!(status["background"]["detail"].is_string());
}

#[test]
fn unconfigured_status_json_has_a_stable_setup_envelope() {
    let temp = setup_fixture();
    let root = temp.path();

    let status: Value = serde_json::from_str(&run(root, &["status", "--format", "json"])).unwrap();

    assert_eq!(status["configured"], false);
    assert_eq!(status["healthy"], false);
    assert_eq!(status["next_action"], "setup");
    assert!(status["inventory"].is_object());
}

#[test]
fn configured_status_failure_still_returns_json_on_stdout() {
    let temp = setup_fixture();
    let root = temp.path();
    write(
        &root.join(".agent-sync/config.toml"),
        "not valid toml = [\n",
    );

    let output = run_output(root, &["status", "--format", "json"]);
    assert!(!output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(status["configured"], true);
    assert_eq!(status["healthy"], false);
    assert_eq!(status["next_action"], "doctor");
    assert!(status["error"].is_string());
    assert!(status["background"].is_object());
}

#[test]
fn no_command_uses_noninteractive_status_without_writing() {
    let temp = setup_fixture();
    let root = temp.path();

    let output = run(root, &[]);

    assert!(output.contains("No managed sync is configured"));
    assert!(!root.join(".agent-sync/config.toml").exists());
}

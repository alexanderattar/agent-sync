use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
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
fn cursor_rule_references_live_codex_guidance_without_copying_it() {
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
    assert!(imported.contains("~/.codex/AGENTS.md"));
    assert!(imported.contains("Cursor-specific settings and rules take precedence"));
    assert!(!imported.contains("credential-bearing-sentinel"));
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
    fs::create_dir_all(root.join(".cursor/projects/example/mcps/qmd")).unwrap();
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
    assert!(error.contains("--portable-only refuses reused pack"));
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
fn raw_mcp_headers_are_not_exported() {
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

    let exported = fs::read_to_string(pack.join("mcp/servers.json")).unwrap();
    assert!(!exported.contains("literal-value-that-should-not-export"));
    assert!(exported.contains("SAFE_AUTH_ENV"));
}

#[test]
fn export_can_limit_mcp_servers_to_a_reviewed_allowlist() {
    let temp = setup_fixture();
    let root = temp.path();
    let pack = root.join("allowlisted-pack");

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

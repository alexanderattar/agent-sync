use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
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
            "--agents-home",
            root.join(".agents").to_str().unwrap(),
        ])
        .args(args)
        .output()
        .expect("run agent-sync");

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
    assert!(output.contains("Shared .agents"));
    assert!(output.contains("shared-style"));
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

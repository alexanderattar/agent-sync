use std::{fs, path::Path};

use agent_sync::{
    apply_pack, export_pack, AgentKind, AgentPaths, ApplyOptions, ExportOptions, SourceSelection,
};
use serde_json::{json, Value};

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn skill(body: &str) -> String {
    format!("---\nname: portable\ndescription: Portable test skill\n---\n\n{body}\n")
}

fn export(paths: &AgentPaths, pack: &Path, source: SourceSelection) {
    export_pack(
        paths,
        pack,
        ExportOptions {
            source,
            include_references: false,
            include_mcp: true,
            mcp_servers: vec!["selected".into(), "conflict".into()],
        },
    )
    .unwrap();
}

fn apply(paths: &AgentPaths, pack: &Path, targets: &[AgentKind]) {
    apply_pack(
        paths,
        pack,
        targets,
        ApplyOptions {
            dry_run: false,
            backup_root: None,
            allow_updates: true,
        },
    )
    .unwrap();
}

#[test]
fn codex_to_amp_preserves_settings_and_user_edits_across_updates() {
    let temp = tempfile::tempdir().unwrap();
    let paths = AgentPaths::for_test(temp.path());
    let source_skill = paths.codex_home.join("skills/portable/SKILL.md");
    write(&source_skill, &skill("Version one"));
    write(
        &paths.codex_home.join("AGENTS.md"),
        "# Personal guidance\n\nVersion one.\n",
    );
    write(
        &paths.codex_home.join("config.toml"),
        concat!(
            "[mcp_servers.selected]\ncommand = \"server-one\"\nenv_vars = [\"API_TOKEN\"]\n",
            "[mcp_servers.conflict]\ncommand = \"canonical\"\n",
            "[mcp_servers.unselected]\ncommand = \"excluded\"\n",
        ),
    );
    let owned = json!({"command":"user-command", "unknown":{"preserve":true}});
    write(
        &paths.amp_config,
        &json!({
            "amp.theme":"dark", "amp.mcpServers":{"conflict":owned.clone()}
        })
        .to_string(),
    );
    let pack = temp.path().join("pack-one");
    export(&paths, &pack, SourceSelection::Codex);
    apply(&paths, &pack, &[AgentKind::Amp]);
    let target_skill = paths.amp_home.join("skills/portable/SKILL.md");
    let target_rule = paths.amp_home.join("AGENTS.md");
    assert_eq!(
        fs::read_to_string(&target_skill).unwrap(),
        skill("Version one")
    );
    assert_eq!(
        fs::read_to_string(&target_rule).unwrap(),
        "# Personal guidance\n\nVersion one.\n"
    );
    let settings: Value = serde_json::from_slice(&fs::read(&paths.amp_config).unwrap()).unwrap();
    assert_eq!(settings["amp.theme"], "dark");
    assert_eq!(settings["amp.mcpServers"]["conflict"], owned);
    assert_eq!(
        settings["amp.mcpServers"]["selected"]["env"]["API_TOKEN"],
        "${API_TOKEN}"
    );
    assert!(settings["amp.mcpServers"].get("unselected").is_none());

    write(&source_skill, &skill("Version two"));
    write(&paths.codex_home.join("AGENTS.md"), "Version two.\n");
    write(&paths.codex_home.join("config.toml"), "[mcp_servers.selected]\ncommand = \"server-two\"\nenv_vars = [\"API_TOKEN\"]\n[mcp_servers.conflict]\ncommand = \"canonical\"\n");
    let pack_two = temp.path().join("pack-two");
    export(&paths, &pack_two, SourceSelection::Codex);
    apply(&paths, &pack_two, &[AgentKind::Amp]);
    assert_eq!(
        fs::read_to_string(&target_skill).unwrap(),
        skill("Version two")
    );
    assert_eq!(fs::read_to_string(&target_rule).unwrap(), "Version two.\n");
    let settings: Value = serde_json::from_slice(&fs::read(&paths.amp_config).unwrap()).unwrap();
    assert_eq!(
        settings["amp.mcpServers"]["selected"]["command"],
        "server-two"
    );
    assert_eq!(settings["amp.mcpServers"]["conflict"], owned);

    write(&target_skill, &skill("User customization"));
    write(&target_rule, "User guidance.\n");
    write(&source_skill, &skill("Version three"));
    write(&paths.codex_home.join("AGENTS.md"), "Version three.\n");
    let pack_three = temp.path().join("pack-three");
    export(&paths, &pack_three, SourceSelection::Codex);
    apply(&paths, &pack_three, &[AgentKind::Amp]);
    assert_eq!(
        fs::read_to_string(&target_skill).unwrap(),
        skill("User customization")
    );
    assert_eq!(
        fs::read_to_string(&target_rule).unwrap(),
        "User guidance.\n"
    );
}

#[test]
fn nested_amp_skills_are_exported_and_preserved_when_importing() {
    let temp = tempfile::tempdir().unwrap();
    let paths = AgentPaths::for_test(temp.path());
    let nested = paths
        .home
        .join(".config/agents/skills/team/workflows/portable/SKILL.md");
    write(&nested, &skill("Nested user skill"));
    let pack = temp.path().join("from-amp");
    export_pack(
        &paths,
        &pack,
        ExportOptions {
            source: SourceSelection::Amp,
            include_references: false,
            include_mcp: false,
            mcp_servers: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(pack.join("skills/portable/SKILL.md")).unwrap(),
        skill("Nested user skill")
    );
    write(
        &paths.codex_home.join("skills/portable/SKILL.md"),
        &skill("Different canonical skill"),
    );
    let incoming = temp.path().join("from-codex");
    export_pack(
        &paths,
        &incoming,
        ExportOptions {
            source: SourceSelection::Codex,
            include_references: false,
            include_mcp: false,
            mcp_servers: Vec::new(),
        },
    )
    .unwrap();
    let report = apply_pack(
        &paths,
        &incoming,
        &[AgentKind::Amp],
        ApplyOptions {
            dry_run: false,
            backup_root: None,
            allow_updates: true,
        },
    )
    .unwrap();
    assert!(report
        .changes
        .iter()
        .any(|change| change.resource == "Skill:portable"
            && change.action == agent_sync::ChangeAction::Skip));
    assert!(!paths.amp_home.join("skills/portable").exists());
    assert_eq!(
        fs::read_to_string(nested).unwrap(),
        skill("Nested user skill")
    );
}

#[test]
fn amp_exports_effective_global_skills_guidance_and_selected_mcp() {
    let temp = tempfile::tempdir().unwrap();
    let paths = AgentPaths::for_test(temp.path());
    for (index, directory) in paths.amp_skill_dirs().unwrap().iter().enumerate() {
        write(
            &directory.join("portable/SKILL.md"),
            &skill(&format!("Priority {index}")),
        );
    }
    write(
        &paths.amp_home.join("AGENTS.md"),
        "Amp personal guidance.\n",
    );
    write(
        &paths.home.join(".config/AGENTS.md"),
        "Shared personal guidance.\n",
    );
    write(
        &paths.amp_config,
        &json!({"amp.mcpServers":{
            "selected":{"command":"portable-server", "env":{"API_TOKEN":"${API_TOKEN}"}},
            "conflict":{"url":"https://example.invalid/mcp", "headers":{"X-Token":"${HTTP_TOKEN}"}},
            "unselected":{"command":"excluded", "unsupported":true}
        }})
        .to_string(),
    );
    let pack = temp.path().join("amp-pack");
    export(&paths, &pack, SourceSelection::Amp);
    assert_eq!(
        fs::read_to_string(pack.join("skills/portable/SKILL.md")).unwrap(),
        skill("Priority 0")
    );
    assert_eq!(
        fs::read_to_string(pack.join("rules/amp-agents.md")).unwrap(),
        "Amp personal guidance.\n\n\nShared personal guidance.\n"
    );

    // Use clean targets so source-compatible Claude fallback skills do not conflict.
    let targets_temp = tempfile::tempdir().unwrap();
    let targets = AgentPaths::for_test(targets_temp.path());
    apply(&targets, &pack, &[AgentKind::Codex, AgentKind::Claude]);
    for destination in [
        targets.codex_home.join("skills/portable/SKILL.md"),
        targets.claude_home.join("skills/portable/SKILL.md"),
    ] {
        assert_eq!(
            fs::read_to_string(destination).unwrap(),
            skill("Priority 0")
        );
    }
    let codex_rules = fs::read_to_string(targets.codex_home.join("AGENTS.md")).unwrap();
    let claude_rules =
        fs::read_to_string(targets.claude_home.join("rules/imported-amp-agents.md")).unwrap();
    assert_eq!(codex_rules, claude_rules);
    assert!(codex_rules.contains("Amp personal guidance."));
    assert!(codex_rules.contains("Shared personal guidance."));
    let codex_mcp = fs::read_to_string(targets.codex_home.join("config.toml")).unwrap();
    let codex_mcp: toml_edit::DocumentMut = codex_mcp.parse().unwrap();
    assert_eq!(
        codex_mcp["mcp_servers"]["selected"]["env_vars"][0].as_str(),
        Some("API_TOKEN")
    );
    assert_eq!(
        codex_mcp["mcp_servers"]["conflict"]["env_http_headers"]["X-Token"].as_str(),
        Some("HTTP_TOKEN")
    );
    assert!(codex_mcp["mcp_servers"].get("unselected").is_none());
    let claude: Value = serde_json::from_slice(&fs::read(&targets.claude_config).unwrap()).unwrap();
    assert_eq!(
        claude["mcpServers"]["selected"]["env"]["API_TOKEN"],
        "${API_TOKEN}"
    );
    assert_eq!(
        claude["mcpServers"]["conflict"]["headers"]["X-Token"],
        "${HTTP_TOKEN}"
    );
    assert!(claude["mcpServers"].get("unselected").is_none());
}

#[test]
fn amp_disabled_compatibility_skill_roots_do_not_export_or_block_imports() {
    let temp = tempfile::tempdir().unwrap();
    let paths = AgentPaths::for_test(temp.path());
    write(
        &paths.home.join(".config/agents/skills/portable/SKILL.md"),
        &skill("Disabled config agents"),
    );
    write(
        &paths.agents_home.join("skills/portable/SKILL.md"),
        &skill("Disabled agents"),
    );
    write(
        &paths.claude_home.join("skills/portable/SKILL.md"),
        &skill("Disabled Claude"),
    );
    write(
        &paths.claude_home.join("skills/claude-only/SKILL.md"),
        &skill("Disabled Claude only"),
    );
    write(
        &paths.amp_config,
        &json!({
            "amp.skills.disableGlobalAgentsSkills": true,
            "amp.skills.disableClaudeCodeSkills": true,
        })
        .to_string(),
    );
    let empty_pack = temp.path().join("disabled-pack");
    export_pack(
        &paths,
        &empty_pack,
        ExportOptions {
            source: SourceSelection::Amp,
            include_references: false,
            include_mcp: false,
            mcp_servers: vec![],
        },
    )
    .unwrap();
    assert!(!empty_pack.join("skills/portable").exists());
    assert!(!empty_pack.join("skills/claude-only").exists());

    write(
        &paths.codex_home.join("skills/portable/SKILL.md"),
        &skill("Imported"),
    );
    let pack = temp.path().join("import-pack");
    export_pack(
        &paths,
        &pack,
        ExportOptions {
            source: SourceSelection::Codex,
            include_references: false,
            include_mcp: false,
            mcp_servers: vec![],
        },
    )
    .unwrap();
    apply(&paths, &pack, &[AgentKind::Amp]);
    assert_eq!(
        fs::read_to_string(paths.amp_home.join("skills/portable/SKILL.md")).unwrap(),
        skill("Imported")
    );
    assert_eq!(
        fs::read_to_string(paths.home.join(".config/agents/skills/portable/SKILL.md")).unwrap(),
        skill("Disabled config agents")
    );
    assert_eq!(
        fs::read_to_string(paths.agents_home.join("skills/portable/SKILL.md")).unwrap(),
        skill("Disabled agents")
    );
    assert_eq!(
        fs::read_to_string(paths.claude_home.join("skills/portable/SKILL.md")).unwrap(),
        skill("Disabled Claude")
    );

    let native_pack = temp.path().join("native-pack");
    export_pack(
        &paths,
        &native_pack,
        ExportOptions {
            source: SourceSelection::Amp,
            include_references: false,
            include_mcp: false,
            mcp_servers: vec![],
        },
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(native_pack.join("skills/portable/SKILL.md")).unwrap(),
        skill("Imported")
    );
}

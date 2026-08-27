mod adapters;
mod agent_skill;
mod apply;
mod config;
mod cursor_history;
mod discover;
mod fsx;
mod manifest;
mod mcp;
mod pack;
mod workflow;

pub use adapters::{AgentKind, AgentPaths};
pub use agent_skill::{
    install_agent_skill, AgentSkillInstallAction, AgentSkillInstallOptions,
    AgentSkillInstallReport, BUNDLED_AGENT_SKILL, BUNDLED_AGENT_SKILL_NAME,
};
pub use apply::{
    apply_pack, diff_pack, format_diff, verify_pack, ApplyOptions, ApplyReport, Change,
    ChangeAction, VerifyReport,
};
pub use config::{
    default_config_path, load_config, render_config, resolve_config_path, save_config,
    CanonicalSource, Config, CursorHistoryConfig, HealthConfig, McpConfig, McpMode,
};
pub use cursor_history::{
    cursor_history_coverage, export_cursor_history, export_cursor_history_from_stdin,
    install_cursor_history_hook, install_cursor_history_hook_with_refresh, qmd_executable,
    qmd_export_is_indexed, qmd_health, qmd_pending_exports, qmd_refresh_last_success,
    refresh_qmd_index, remove_cursor_history_hook, sweep_cursor_history, CursorHistoryCoverage,
    CursorHistoryInstallReport, CursorHistoryRemoveReport, QmdHealth,
};
pub use discover::{discover, AgentInventory, Inventory};
pub use pack::{export_pack, init_pack, ExportOptions, ExportReport, InitReport, SourceSelection};
pub use workflow::{
    doctor_managed, setup_managed, status_managed, sync_managed, ChangeCounts, HealthReport,
    RunRecord, RunResult, SetupOptions, SetupReport, SyncOptions, SyncReport,
};

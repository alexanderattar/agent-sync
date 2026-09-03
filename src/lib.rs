mod adapters;
mod agent_skill;
mod apply;
mod config;
mod cursor_history;
mod discover;
mod fsx;
mod manifest;
mod mcp;
mod ownership;
mod pack;
mod schedule;
pub mod tui;
mod workflow;

pub use fsx::{install_staged_executable_if_unchanged, remove_installed_executable_if_unchanged};

pub use adapters::{AgentKind, AgentPaths};
pub use agent_skill::{
    claude_skill_destination, claude_skill_state_path, install_agent_skill, install_agent_skills,
    AgentSkillInstallAction, AgentSkillInstallOptions, AgentSkillInstallReport,
    AgentSkillInstallSetReport, BUNDLED_AGENT_SKILL, BUNDLED_AGENT_SKILL_NAME,
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
    cursor_history_coverage, cursor_history_coverage_at, default_cursor_history_output_dir,
    ensure_qmd_collection, export_cursor_history, export_cursor_history_from_stdin,
    install_cursor_history_hook, install_cursor_history_hook_with_refresh, qmd_executable,
    qmd_export_is_indexed, qmd_health, qmd_missing_exports, qmd_pending_exports,
    qmd_refresh_last_success, refresh_qmd_index, refresh_qmd_index_for_output,
    remove_cursor_history_hook, run_deferred_qmd_refresh, sweep_cursor_history,
    sweep_cursor_history_to, CursorHistoryCoverage, CursorHistoryInstallReport,
    CursorHistoryRemoveReport, QmdHealth, QMD_CURSOR_COLLECTION,
};
pub use discover::{discover, AgentInventory, Inventory};
pub use pack::{export_pack, init_pack, ExportOptions, ExportReport, InitReport, SourceSelection};
pub use schedule::{
    install_schedule, render_launch_agent, schedule_status, uninstall_schedule,
    LaunchAgentController, ScheduleAction, ScheduleError, ScheduleOperation, ScheduleReport,
    ScheduleResult, ScheduleSpec, SystemLaunchAgentController, DEFAULT_INTERVAL_SECONDS,
    LAUNCH_AGENT_LABEL,
};
pub use workflow::{
    doctor_managed, setup_managed, status_managed, status_managed_report, sync_managed,
    ChangeCounts, CursorHistoryMode, HealthReport, ManagedStatus, RunRecord, RunResult,
    SetupOptions, SetupReport, StatusLastSuccess, StatusNextAction, SyncOptions, SyncReport,
};

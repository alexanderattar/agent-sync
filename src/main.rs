use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use agent_sync::tui::{
    self, ActionItem, Agent as TuiAgent, AgentMcpServers, CursorHistoryChoice, HealthState,
    McpChoice, SetupScreen, SetupSelection, StatusLine, StatusScreen, Tone, TuiOutcome, TuiRequest,
};
use agent_sync::{
    apply_pack, default_cursor_history_output_dir, discover, doctor_managed,
    export_cursor_history_from_stdin, export_pack, format_diff, init_pack,
    install_cursor_history_hook, install_schedule, install_staged_executable_if_unchanged,
    qmd_executable, remove_installed_executable_if_unchanged, resolve_config_path, schedule_status,
    setup_managed, status_managed_report, sync_managed, uninstall_schedule, verify_pack, AgentKind,
    AgentPaths, ApplyOptions, CanonicalSource, Config, CursorHistoryMode, ExportOptions, McpMode,
    ScheduleAction, ScheduleReport, ScheduleSpec, SetupOptions, SourceSelection, StatusNextAction,
    SyncOptions, SystemLaunchAgentController, QMD_CURSOR_COLLECTION,
};

#[derive(Debug, Parser)]
#[command(name = "agent-sync")]
#[command(about = "Synchronize personal agent tooling across local coding agents")]
#[command(version)]
struct Cli {
    #[command(flatten)]
    paths: PathArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, clap::Args)]
struct PathArgs {
    #[arg(long, env = "AGENT_SYNC_CONFIG")]
    config: Option<PathBuf>,

    #[arg(long, env = "AGENT_SYNC_HOME")]
    home: Option<PathBuf>,

    #[arg(long, env = "AGENT_SYNC_CODEX_HOME")]
    codex_home: Option<PathBuf>,

    #[arg(long, env = "AGENT_SYNC_CLAUDE_HOME")]
    claude_home: Option<PathBuf>,

    #[arg(long, env = "AGENT_SYNC_CLAUDE_CONFIG")]
    claude_config: Option<PathBuf>,

    #[arg(long, env = "AGENT_SYNC_CURSOR_HOME")]
    cursor_home: Option<PathBuf>,

    #[arg(long, env = "AGENT_SYNC_CURSOR_CONFIG")]
    cursor_config: Option<PathBuf>,

    #[arg(long, env = "AGENT_SYNC_AGENTS_HOME")]
    agents_home: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(name = "__install-commit", hide = true)]
    InstallCommit(InstallCommitArgs),
    #[command(name = "__install-remove", hide = true)]
    InstallRemove(InstallRemoveArgs),
    /// Configure a safe managed sync and install its natural-language skill
    Setup(SetupArgs),
    /// Preview or apply the configured sync, then verify and record it
    Sync(SyncArgs),
    /// Run a comprehensive read-only health and drift check
    Doctor,
    /// Initialize an empty low-level pack
    Init(PackArgs),
    /// Show managed health, inventory, or a low-level pack diff
    Status(StatusArgs),
    /// List the agent resources visible on this machine
    Discover(DiscoverArgs),
    /// Export resources to a low-level pack
    Export(ExportArgs),
    /// Compare a low-level pack with one or more agents
    Diff(PackTargetArgs),
    /// Preview or apply a low-level pack
    Apply(ApplyArgs),
    /// Verify a low-level pack against one or more agents
    Verify(PackTargetArgs),
    /// Install or run the Cursor chat export hook
    CursorHistory {
        #[command(subcommand)]
        command: CursorHistoryCommand,
    },
    /// Manage the background sync schedule on macOS
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
}

#[derive(Debug, clap::Args)]
struct InstallCommitArgs {
    #[arg(long)]
    target: PathBuf,

    #[arg(long)]
    expected_sha256: Option<String>,
}

#[derive(Debug, clap::Args)]
struct InstallRemoveArgs {
    #[arg(long)]
    target: PathBuf,

    #[arg(long)]
    expected_sha256: String,
}

#[derive(Debug, clap::Args)]
struct SetupArgs {
    #[arg(long = "from", value_enum)]
    source: Option<ManagedSourceArg>,

    #[arg(long = "to")]
    targets: Option<String>,

    #[arg(
        long,
        conflicts_with = "all_mcp",
        conflicts_with = "no_mcp",
        help = "Sync only these reviewed MCP server names"
    )]
    mcp_servers: Option<String>,

    #[arg(
        long,
        conflicts_with = "no_mcp",
        help = "Sync every source MCP server; use only after review"
    )]
    all_mcp: bool,

    #[arg(long, help = "Disable MCP syncing")]
    no_mcp: bool,

    #[arg(long, conflicts_with = "exclude_references")]
    include_references: bool,

    #[arg(long, help = "Exclude memory and automation references")]
    exclude_references: bool,

    #[arg(long, conflicts_with = "block_updates")]
    allow_updates: bool,

    #[arg(long, help = "Block replacement of target-owned content")]
    block_updates: bool,

    #[arg(
        long,
        conflicts_with = "no_cursor_history",
        help = "Export Cursor chats and keep the hook repaired"
    )]
    cursor_history: bool,

    #[arg(
        long,
        conflicts_with = "refresh_qmd",
        help = "Disable the managed Cursor history hook"
    )]
    no_cursor_history: bool,

    #[arg(
        long,
        conflicts_with = "refresh_qmd",
        help = "Disable QMD refresh when Cursor history is enabled"
    )]
    skip_qmd: bool,

    #[arg(long, help = "Enable QMD refresh for Cursor history exports")]
    refresh_qmd: bool,

    #[arg(long)]
    stale_after_hours: Option<u64>,

    #[arg(long, help = "Write the managed config and bundled skill")]
    yes: bool,
}

#[derive(Debug, clap::Args)]
struct SyncArgs {
    #[arg(long, help = "Apply the preview, verify it, and record the run")]
    yes: bool,

    #[arg(long, requires = "yes", help = "Use scheduled-task output conventions")]
    automation: bool,
}

#[derive(Debug, clap::Args)]
struct PackArgs {
    #[arg(long)]
    pack: PathBuf,
}

#[derive(Debug, clap::Args)]
struct StatusArgs {
    #[arg(long)]
    pack: Option<PathBuf>,

    #[arg(long, default_value = "codex,claude")]
    targets: String,

    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
}

impl Default for StatusArgs {
    fn default() -> Self {
        Self {
            pack: None,
            targets: "codex,claude".to_string(),
            format: OutputFormat::Text,
        }
    }
}

#[derive(Debug, clap::Args)]
struct DiscoverArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
}

#[derive(Debug, clap::Args)]
struct ExportArgs {
    #[arg(long)]
    pack: PathBuf,

    #[arg(long = "from", value_enum, default_value_t = SourceArg::All)]
    source: SourceArg,

    #[arg(
        long,
        help = "Exclude memory and automation references from the exported pack"
    )]
    portable_only: bool,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Export only these MCP server names"
    )]
    mcp_servers: Vec<String>,
}

#[derive(Debug, clap::Args)]
struct PackTargetArgs {
    #[arg(long)]
    pack: PathBuf,

    #[arg(long, default_value = "codex,claude")]
    targets: String,
}

#[derive(Debug, clap::Args)]
struct ApplyArgs {
    #[arg(long)]
    pack: PathBuf,

    #[arg(long, default_value = "codex,claude")]
    targets: String,

    #[arg(
        long,
        help = "Actually write changes. Without this, apply prints the plan only."
    )]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum CursorHistoryCommand {
    Install {
        #[arg(long)]
        executable: Option<PathBuf>,

        #[arg(
            long,
            help = "Actually write the additive hook. Without this, print the plan only."
        )]
        yes: bool,
    },
    Export {
        #[arg(long)]
        output_dir: Option<PathBuf>,

        #[arg(long, hide = true)]
        skip_qmd: bool,
    },
    #[command(hide = true)]
    RefreshQmd {
        #[arg(long)]
        output_dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ScheduleCommand {
    /// Show whether the managed background job is installed and loaded
    Status,
    /// Preview or install the managed background job
    Install {
        #[arg(long, help = "Install and load the previewed background job")]
        yes: bool,
    },
    /// Preview or remove the managed background job
    Uninstall {
        #[arg(long, help = "Unload and remove the managed background job")]
        yes: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SourceArg {
    All,
    Codex,
    Claude,
    Cursor,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ManagedSourceArg {
    Codex,
    Claude,
    Cursor,
}

struct AppContext {
    paths: AgentPaths,
    config_path: PathBuf,
}

#[derive(Serialize)]
struct CliManagedStatus<'a> {
    configured: bool,
    #[serde(flatten)]
    managed: &'a agent_sync::ManagedStatus,
    background: &'a BackgroundStatus,
}

#[derive(Serialize)]
struct CliUnconfiguredStatus<'a> {
    configured: bool,
    healthy: bool,
    next_action: &'static str,
    inventory: &'a agent_sync::Inventory,
}

#[derive(Serialize)]
struct CliStatusFailure<'a> {
    configured: bool,
    healthy: bool,
    next_action: &'static str,
    error: String,
    background: &'a BackgroundStatus,
}

#[derive(Serialize)]
struct BackgroundStatus {
    supported: bool,
    enabled: bool,
    healthy: Option<bool>,
    action: Option<String>,
    detail: String,
    log_dir: Option<PathBuf>,
}

impl BackgroundStatus {
    fn to_text(&self) -> String {
        if !self.supported {
            return format!("Background: unavailable ({})\n", self.detail);
        }
        if self.enabled && self.healthy == Some(true) {
            return "Background: daily sync is running\n".to_string();
        }
        if self.action.as_deref() == Some("add") {
            return "Background: off\n".to_string();
        }
        format!("Background: needs attention ({})\n", self.detail)
    }
}

struct BackgroundInspection {
    status: BackgroundStatus,
    report: Option<ScheduleReport>,
}

impl AppContext {
    fn from_path_args(args: PathArgs) -> Result<Self> {
        let config_override = args.config;
        let paths = AgentPaths::from_optional(
            args.home,
            args.codex_home,
            args.claude_home,
            args.claude_config,
            args.cursor_home,
            args.cursor_config,
            args.agents_home,
        )?;
        let config_path = resolve_config_path(&paths.home, config_override.as_deref());
        Ok(Self { paths, config_path })
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let context = AppContext::from_path_args(cli.paths)?;
    match cli.command {
        Some(command) => run_command(command, &context),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => {
            run_interactive(&context)
        }
        None => run_status(&context, StatusArgs::default()),
    }
}

fn run_command(command: Command, context: &AppContext) -> Result<()> {
    match command {
        Command::InstallCommit(args) => run_install_commit(args),
        Command::InstallRemove(args) => run_install_remove(args),
        Command::Setup(args) => run_setup(context, args),
        Command::Sync(args) => run_sync(context, args),
        Command::Doctor => run_doctor(context),
        Command::Init(args) => run_init(args),
        Command::Status(args) => run_status(context, args),
        Command::Discover(args) => run_discover(context, args),
        Command::Export(args) => run_export(context, args),
        Command::Diff(args) => run_diff(context, args),
        Command::Apply(args) => run_apply(context, args),
        Command::Verify(args) => run_verify(context, args),
        Command::CursorHistory { command } => run_cursor_history(context, command),
        Command::Schedule { command } => run_schedule(context, command),
    }
}

fn run_install_commit(args: InstallCommitArgs) -> Result<()> {
    let staged = std::env::current_exe().context("resolve staged installer executable")?;
    install_staged_executable_if_unchanged(&staged, &args.target, args.expected_sha256.as_deref())
}

fn run_install_remove(args: InstallRemoveArgs) -> Result<()> {
    remove_installed_executable_if_unchanged(&args.target, &args.expected_sha256)
}

fn run_interactive(context: &AppContext) -> Result<()> {
    if context.config_path.exists() {
        run_interactive_status(context)
    } else {
        run_interactive_setup(context)
    }
}

fn run_interactive_status(context: &AppContext) -> Result<()> {
    let screen = build_status_screen(context)?;
    match tui::run(TuiRequest::Status(screen)).context("run agent-sync status UI")? {
        TuiOutcome::Cancelled => Ok(()),
        TuiOutcome::Action(action) => run_interactive_action(context, &action),
        TuiOutcome::Setup(_) => anyhow::bail!("status UI returned an unexpected setup selection"),
    }
}

fn build_status_screen(context: &AppContext) -> Result<StatusScreen> {
    let executable = std::env::current_exe()?;
    let mut status = status_managed_report(&context.paths, &context.config_path, &executable)?;
    let background = inspect_background(context, &executable);
    merge_background_health(&mut status, &background.status);
    let mut summary = vec![
        StatusLine {
            label: "Drift".to_string(),
            value: format!(
                "{} add · {} update · {} preserved",
                status.drift.add, status.drift.update, status.drift.preserved
            ),
            tone: if status.drift.add > 0 || status.drift.update > 0 {
                Tone::Attention
            } else {
                Tone::Healthy
            },
        },
        StatusLine {
            label: "Last sync".to_string(),
            value: status.last_success.as_ref().map_or_else(
                || "never".to_string(),
                |record| record.finished_at.to_rfc3339(),
            ),
            tone: if status.last_success.is_some() {
                Tone::Normal
            } else {
                Tone::Attention
            },
        },
        StatusLine {
            label: "Cursor history".to_string(),
            value: cursor_history_label(status.cursor_history).to_string(),
            tone: Tone::Muted,
        },
    ];
    summary.push(background_status_line(&background.status));

    let mut actions = vec![
        ActionItem {
            id: "sync-now".to_string(),
            label: "Sync now".to_string(),
            detail: "Back up, apply, verify, and record the managed plan.".to_string(),
            tone: if status.drift.add > 0 || status.drift.update > 0 {
                Tone::Attention
            } else {
                Tone::Normal
            },
        },
        ActionItem {
            id: "preview-sync".to_string(),
            label: "Preview sync".to_string(),
            detail: "Show the complete plan without writing files.".to_string(),
            tone: Tone::Muted,
        },
        ActionItem {
            id: "doctor".to_string(),
            label: "Run health check".to_string(),
            detail: "Inspect paths, policy, history, drift, and run state.".to_string(),
            tone: if status.healthy {
                Tone::Muted
            } else {
                Tone::Error
            },
        },
        ActionItem {
            id: "setup".to_string(),
            label: "Change setup".to_string(),
            detail: "Review the source, targets, MCP access, and history policy.".to_string(),
            tone: Tone::Muted,
        },
    ];
    add_schedule_action(
        &mut actions,
        background.report.as_ref(),
        status.last_success.is_some(),
    );

    let health = if !status.healthy {
        HealthState::Error
    } else if status.warnings.is_empty() {
        HealthState::Healthy
    } else {
        HealthState::Attention
    };
    let message = match status.next_action {
        StatusNextAction::PreviewSync => Some("Managed drift is ready to review.".to_string()),
        StatusNextAction::RunDoctor => status.errors.first().cloned(),
        StatusNextAction::None => status.warnings.first().cloned(),
    };

    Ok(StatusScreen {
        source: Some(tui_agent_from_kind(status.source.agent_kind())),
        targets: status
            .targets
            .iter()
            .copied()
            .map(tui_agent_from_kind)
            .collect(),
        health,
        summary,
        actions,
        message,
    })
}

fn background_status_line(background: &BackgroundStatus) -> StatusLine {
    match (background.supported, background.enabled, background.healthy) {
        (true, true, Some(true)) => StatusLine {
            label: "Background".to_string(),
            value: "daily sync is running".to_string(),
            tone: Tone::Healthy,
        },
        (true, false, Some(false)) if background.action.as_deref() == Some("add") => StatusLine {
            label: "Background".to_string(),
            value: "off".to_string(),
            tone: Tone::Muted,
        },
        (true, _, _) => StatusLine {
            label: "Background".to_string(),
            value: background.detail.clone(),
            tone: Tone::Attention,
        },
        (false, _, _) => StatusLine {
            label: "Background".to_string(),
            value: "manual on this platform".to_string(),
            tone: Tone::Muted,
        },
    }
}

fn merge_background_health(status: &mut agent_sync::ManagedStatus, background: &BackgroundStatus) {
    let intentionally_disabled = background.action.as_deref() == Some("add");
    if background.supported && background.healthy == Some(false) && !intentionally_disabled {
        status.healthy = false;
        let error = format!("background sync: {}", background.detail);
        if !status.errors.contains(&error) {
            status.errors.push(error);
        }
        if status.next_action == StatusNextAction::None {
            status.next_action = StatusNextAction::RunDoctor;
        }
    }
}

fn add_schedule_action(
    actions: &mut Vec<ActionItem>,
    schedule: Option<&ScheduleReport>,
    has_successful_sync: bool,
) {
    let Some(schedule) = schedule else {
        return;
    };
    if schedule.healthy {
        actions.push(ActionItem {
            id: "schedule-disable".to_string(),
            label: "Disable background sync".to_string(),
            detail: "Preview removal of the managed daily job.".to_string(),
            tone: Tone::Muted,
        });
    } else if has_successful_sync && schedule.action != ScheduleAction::Conflict {
        actions.push(ActionItem {
            id: "schedule-enable".to_string(),
            label: "Enable daily background sync".to_string(),
            detail: "Preview a user-owned job that runs the verified sync workflow.".to_string(),
            tone: Tone::Normal,
        });
    }
}

fn inspect_background(context: &AppContext, executable: &Path) -> BackgroundInspection {
    if cfg!(debug_assertions) && std::env::var_os("AGENT_SYNC_TEST_DISABLE_BACKGROUND").is_some() {
        return BackgroundInspection {
            status: BackgroundStatus {
                supported: false,
                enabled: false,
                healthy: None,
                action: None,
                detail: "background inspection is disabled for this test process".to_string(),
                log_dir: None,
            },
            report: None,
        };
    }
    if !cfg!(target_os = "macos") {
        return BackgroundInspection {
            status: BackgroundStatus {
                supported: false,
                enabled: false,
                healthy: None,
                action: None,
                detail: "managed schedules currently require macOS".to_string(),
                log_dir: None,
            },
            report: None,
        };
    }
    let spec = match schedule_spec(context, executable) {
        Ok(spec) => spec,
        Err(error) => {
            return BackgroundInspection {
                status: BackgroundStatus {
                    supported: true,
                    enabled: false,
                    healthy: Some(false),
                    action: None,
                    detail: error.to_string(),
                    log_dir: Some(context.paths.home.join(".agent-sync/logs")),
                },
                report: None,
            };
        }
    };
    let result = SystemLaunchAgentController::for_current_user()
        .and_then(|mut controller| schedule_status(&spec, &mut controller));
    match result {
        Ok(report) => BackgroundInspection {
            status: BackgroundStatus {
                supported: true,
                enabled: report.loaded,
                healthy: Some(report.healthy),
                action: Some(report.action.to_string()),
                detail: report.detail.clone(),
                log_dir: Some(report.log_dir.clone()),
            },
            report: Some(report),
        },
        Err(error) => BackgroundInspection {
            status: BackgroundStatus {
                supported: true,
                enabled: false,
                healthy: Some(false),
                action: None,
                detail: error.to_string(),
                log_dir: Some(spec.log_dir()),
            },
            report: None,
        },
    }
}

fn run_interactive_action(context: &AppContext, action: &str) -> Result<()> {
    match action {
        "sync-now" => run_sync(
            context,
            SyncArgs {
                yes: true,
                automation: false,
            },
        ),
        "preview-sync" => run_sync(
            context,
            SyncArgs {
                yes: false,
                automation: false,
            },
        ),
        "doctor" => run_doctor(context),
        "setup" => run_interactive_setup(context),
        "schedule-enable" => confirm_interactive_schedule(context, true),
        "schedule-disable" => confirm_interactive_schedule(context, false),
        other => anyhow::bail!("unknown interactive action `{other}`"),
    }
}

fn run_interactive_setup(context: &AppContext) -> Result<()> {
    let mut config = if context.config_path.exists() {
        agent_sync::load_config(&context.config_path)?
    } else {
        Config::default()
    };
    let mut error = None;
    loop {
        let screen = build_setup_screen(context, &config, error.take())?;
        let selection = match tui::run(TuiRequest::Setup(screen)).context("run setup UI")? {
            TuiOutcome::Cancelled => return Ok(()),
            TuiOutcome::Setup(selection) => selection,
            TuiOutcome::Action(_) => anyhow::bail!("setup UI returned an unexpected action"),
        };
        config = config_from_setup_selection(config, selection)?;
        let preview = match setup_managed(
            &context.paths,
            &context.config_path,
            &config,
            SetupOptions { dry_run: true },
        ) {
            Ok(preview) => preview,
            Err(setup_error) => {
                error = Some(format!("{setup_error:#}"));
                continue;
            }
        };
        let confirmation = setup_confirmation_screen(&context.paths, &config, &preview);
        match tui::run(TuiRequest::Status(confirmation)).context("run setup confirmation UI")? {
            TuiOutcome::Action(action) if action == "save-setup" => {
                let report = setup_managed(
                    &context.paths,
                    &context.config_path,
                    &config,
                    SetupOptions { dry_run: false },
                )?;
                print!("{}", report.to_text());
                return continue_after_setup(context);
            }
            TuiOutcome::Cancelled | TuiOutcome::Action(_) => {}
            TuiOutcome::Setup(_) => {
                anyhow::bail!("setup confirmation returned an unexpected selection")
            }
        }
    }
}

fn continue_after_setup(context: &AppContext) -> Result<()> {
    let executable = std::env::current_exe()?;
    let preview = sync_managed(
        &context.paths,
        &context.config_path,
        &executable,
        SyncOptions {
            dry_run: true,
            automation: false,
        },
    )?;
    let screen = StatusScreen {
        source: Some(tui_agent_from_kind(preview.record.source.agent_kind())),
        targets: preview
            .record
            .targets
            .iter()
            .copied()
            .map(tui_agent_from_kind)
            .collect(),
        health: HealthState::Unknown,
        summary: vec![
            StatusLine {
                label: "First sync".to_string(),
                value: format!(
                    "{} add · {} update · {} preserved",
                    preview.record.before.add,
                    preview.record.before.update,
                    preview.record.before.preserved
                ),
                tone: if preview.record.before.update > 0 {
                    Tone::Attention
                } else {
                    Tone::Healthy
                },
            },
            StatusLine {
                label: "Verification".to_string(),
                value: "runs after the changes are applied".to_string(),
                tone: Tone::Muted,
            },
        ],
        actions: vec![
            ActionItem {
                id: "apply-first-sync".to_string(),
                label: "Apply first sync".to_string(),
                detail: "Back up, apply, verify, and record the reviewed route.".to_string(),
                tone: Tone::Healthy,
            },
            ActionItem {
                id: "finish-later".to_string(),
                label: "Finish later".to_string(),
                detail: "Keep setup saved without changing target agents.".to_string(),
                tone: Tone::Muted,
            },
        ],
        message: Some(
            "Setup is saved. Complete the first sync to enable background maintenance.".to_string(),
        ),
    };
    let outcome = tui::run(TuiRequest::Status(screen)).context("run first sync confirmation UI")?;
    if !matches!(outcome, TuiOutcome::Action(action) if action == "apply-first-sync") {
        return Ok(());
    }

    let applied = sync_managed(
        &context.paths,
        &context.config_path,
        &executable,
        SyncOptions {
            dry_run: false,
            automation: false,
        },
    )?;
    print!("{}", applied.to_text());
    if cfg!(target_os = "macos") {
        confirm_interactive_schedule(context, true)?;
    }
    Ok(())
}

fn build_setup_screen(
    context: &AppContext,
    config: &Config,
    error: Option<String>,
) -> Result<SetupScreen> {
    let inventory = discover(&context.paths)?;
    Ok(SetupScreen {
        available_sources: available_source_agents(&context.paths),
        target_agents: vec![TuiAgent::Codex, TuiAgent::Claude, TuiAgent::Cursor],
        preserve_initial_source: context.config_path.exists(),
        mcp_servers: vec![
            AgentMcpServers {
                agent: TuiAgent::Codex,
                servers: inventory.codex.mcp_servers,
            },
            AgentMcpServers {
                agent: TuiAgent::Claude,
                servers: inventory.claude.mcp_servers,
            },
            AgentMcpServers {
                agent: TuiAgent::Cursor,
                servers: inventory.cursor.mcp_servers,
            },
        ],
        qmd_available: qmd_executable(&context.paths).is_some(),
        include_references: config.include_references,
        initial: setup_selection_from_config(config),
        error,
    })
}

fn available_source_agents(paths: &AgentPaths) -> Vec<TuiAgent> {
    [
        (TuiAgent::Codex, paths.codex_home.as_path()),
        (TuiAgent::Claude, paths.claude_home.as_path()),
        (TuiAgent::Cursor, paths.cursor_home.as_path()),
    ]
    .into_iter()
    .filter_map(|(agent, path)| path.is_dir().then_some(agent))
    .collect()
}

fn setup_selection_from_config(config: &Config) -> SetupSelection {
    let mcp = match config.mcp.mode {
        McpMode::None => McpChoice::None,
        McpMode::Selected => McpChoice::Selected(config.mcp.servers.clone()),
        McpMode::All => McpChoice::All,
    };
    let cursor_history = match (
        config.cursor_history.enabled,
        config.cursor_history.refresh_qmd,
    ) {
        (false, _) => CursorHistoryChoice::Disabled,
        (true, false) => CursorHistoryChoice::ExportOnly,
        (true, true) => CursorHistoryChoice::ExportAndQmd,
    };
    SetupSelection {
        source: tui_agent_from_kind(config.source.agent_kind()),
        targets: config
            .targets
            .iter()
            .copied()
            .map(tui_agent_from_kind)
            .collect(),
        mcp,
        cursor_history,
    }
}

fn config_from_setup_selection(mut config: Config, selection: SetupSelection) -> Result<Config> {
    config.source = canonical_source_from_tui(selection.source)?;
    config.targets = selection
        .targets
        .into_iter()
        .map(agent_kind_from_tui)
        .collect();
    match selection.mcp {
        McpChoice::None => {
            config.mcp.mode = McpMode::None;
            config.mcp.servers.clear();
        }
        McpChoice::Selected(servers) => {
            config.mcp.mode = McpMode::Selected;
            config.mcp.servers = servers;
        }
        McpChoice::All => {
            config.mcp.mode = McpMode::All;
            config.mcp.servers.clear();
        }
    }
    match selection.cursor_history {
        CursorHistoryChoice::Disabled => {
            config.cursor_history.enabled = false;
            config.cursor_history.refresh_qmd = false;
        }
        CursorHistoryChoice::ExportOnly => {
            config.cursor_history.enabled = true;
            config.cursor_history.refresh_qmd = false;
        }
        CursorHistoryChoice::ExportAndQmd => {
            config.cursor_history.enabled = true;
            config.cursor_history.refresh_qmd = true;
        }
    }
    config.validate()?;
    Ok(config)
}

fn setup_confirmation_screen(
    paths: &AgentPaths,
    config: &Config,
    preview: &agent_sync::SetupReport,
) -> StatusScreen {
    let mcp = match config.mcp.mode {
        McpMode::None => "none".to_string(),
        McpMode::Selected => config.mcp.servers.join(", "),
        McpMode::All => "all source servers".to_string(),
    };
    let mut summary = vec![
        StatusLine {
            label: "MCP".to_string(),
            value: mcp,
            tone: if config.mcp.mode == McpMode::All {
                Tone::Attention
            } else {
                Tone::Normal
            },
        },
        StatusLine {
            label: "Cursor history".to_string(),
            value: cursor_history_label(cursor_history_mode_from_config(config)).to_string(),
            tone: Tone::Muted,
        },
        StatusLine {
            label: "References".to_string(),
            value: if config.include_references {
                "included"
            } else {
                "excluded"
            }
            .to_string(),
            tone: if config.include_references {
                Tone::Attention
            } else {
                Tone::Healthy
            },
        },
    ];
    if config.cursor_history.enabled {
        let destination = default_cursor_history_output_dir(paths);
        summary.push(StatusLine {
            label: "Private chat copies".to_string(),
            value: if config.cursor_history.refresh_qmd {
                format!("{} · qmd://{QMD_CURSOR_COLLECTION}/", destination.display())
            } else {
                destination.display().to_string()
            },
            tone: Tone::Attention,
        });
    }
    summary.extend([
        StatusLine {
            label: "Target content".to_string(),
            value: if config.allow_updates {
                "managed replacements allowed"
            } else {
                "existing content preserved"
            }
            .to_string(),
            tone: if config.allow_updates {
                Tone::Attention
            } else {
                Tone::Healthy
            },
        },
        StatusLine {
            label: "Config".to_string(),
            value: if preview.config_changed {
                "will be saved"
            } else {
                "already matches"
            }
            .to_string(),
            tone: Tone::Muted,
        },
    ]);
    StatusScreen {
        source: Some(tui_agent_from_kind(config.source.agent_kind())),
        targets: config
            .targets
            .iter()
            .copied()
            .map(tui_agent_from_kind)
            .collect(),
        health: HealthState::Unknown,
        summary,
        actions: vec![
            ActionItem {
                id: "save-setup".to_string(),
                label: "Save setup".to_string(),
                detail: "Write the reviewed config and install the natural-language skill."
                    .to_string(),
                tone: Tone::Healthy,
            },
            ActionItem {
                id: "back".to_string(),
                label: "Go back".to_string(),
                detail: "Change the selection without writing files.".to_string(),
                tone: Tone::Muted,
            },
        ],
        message: Some("Preview complete. No files have been written.".to_string()),
    }
}

fn confirm_interactive_schedule(context: &AppContext, install: bool) -> Result<()> {
    if install {
        ensure_schedule_ready(context)?;
    }
    let executable = std::env::current_exe()?;
    let spec = schedule_spec(context, executable)?;
    let mut controller = SystemLaunchAgentController::for_current_user()?;
    let preview = if install {
        install_schedule(&spec, &mut controller, true)?
    } else {
        uninstall_schedule(&spec, &mut controller, true)?
    };
    let action_id = if install {
        "confirm-schedule-install"
    } else {
        "confirm-schedule-uninstall"
    };
    let screen = StatusScreen {
        source: None,
        targets: Vec::new(),
        health: HealthState::Unknown,
        summary: vec![
            StatusLine {
                label: "Plan".to_string(),
                value: preview.detail.clone(),
                tone: Tone::Normal,
            },
            StatusLine {
                label: "Logs".to_string(),
                value: preview.log_dir.display().to_string(),
                tone: Tone::Muted,
            },
        ],
        actions: vec![
            ActionItem {
                id: action_id.to_string(),
                label: if install {
                    "Enable daily sync"
                } else {
                    "Disable daily sync"
                }
                .to_string(),
                detail: "Apply this reviewed schedule change.".to_string(),
                tone: if install {
                    Tone::Healthy
                } else {
                    Tone::Attention
                },
            },
            ActionItem {
                id: "cancel".to_string(),
                label: "Cancel".to_string(),
                detail: "Leave the current schedule unchanged.".to_string(),
                tone: Tone::Muted,
            },
        ],
        message: Some("Schedule preview complete. No files have been written.".to_string()),
    };
    let outcome = tui::run(TuiRequest::Status(screen)).context("run schedule confirmation UI")?;
    let confirmed = matches!(
        outcome,
        TuiOutcome::Action(ref selected) if selected == action_id
    );
    if !confirmed {
        return Ok(());
    }
    let report = if install {
        install_schedule(&spec, &mut controller, false)?
    } else {
        uninstall_schedule(&spec, &mut controller, false)?
    };
    print!("{}", report.to_text());
    Ok(())
}

fn tui_agent_from_kind(agent: AgentKind) -> TuiAgent {
    match agent {
        AgentKind::Codex => TuiAgent::Codex,
        AgentKind::Claude => TuiAgent::Claude,
        AgentKind::Cursor => TuiAgent::Cursor,
    }
}

fn agent_kind_from_tui(agent: TuiAgent) -> AgentKind {
    match agent {
        TuiAgent::Codex => AgentKind::Codex,
        TuiAgent::Claude => AgentKind::Claude,
        TuiAgent::Cursor => AgentKind::Cursor,
    }
}

fn canonical_source_from_tui(agent: TuiAgent) -> Result<CanonicalSource> {
    match agent {
        TuiAgent::Codex => Ok(CanonicalSource::Codex),
        TuiAgent::Claude => Ok(CanonicalSource::Claude),
        TuiAgent::Cursor => Ok(CanonicalSource::Cursor),
    }
}

fn cursor_history_label(mode: CursorHistoryMode) -> &'static str {
    match mode {
        CursorHistoryMode::Disabled => "disabled",
        CursorHistoryMode::ExportOnly => "exports only",
        CursorHistoryMode::Qmd => "exports and QMD",
    }
}

fn cursor_history_mode_from_config(config: &Config) -> CursorHistoryMode {
    match (
        config.cursor_history.enabled,
        config.cursor_history.refresh_qmd,
    ) {
        (false, _) => CursorHistoryMode::Disabled,
        (true, false) => CursorHistoryMode::ExportOnly,
        (true, true) => CursorHistoryMode::Qmd,
    }
}

fn run_setup(context: &AppContext, args: SetupArgs) -> Result<()> {
    let config = build_setup_config(&context.config_path, &args)?;
    let report = setup_managed(
        &context.paths,
        &context.config_path,
        &config,
        SetupOptions { dry_run: !args.yes },
    )?;
    print!("{}", report.to_text());
    Ok(())
}

fn build_setup_config(config_path: &Path, args: &SetupArgs) -> Result<Config> {
    let mut config = if config_path.exists() {
        agent_sync::load_config(config_path)?
    } else {
        Config::default()
    };
    apply_route_options(&mut config, args)?;
    apply_mcp_options(&mut config, args)?;
    apply_content_options(&mut config, args);
    apply_cursor_history_options(&mut config, args);
    if let Some(stale_after_hours) = args.stale_after_hours {
        config.health.stale_after_hours = stale_after_hours;
    }
    Ok(config)
}

fn apply_route_options(config: &mut Config, args: &SetupArgs) -> Result<()> {
    if let Some(source) = args.source {
        config.source = source.into();
    }
    if let Some(targets) = &args.targets {
        config.targets = parse_targets(targets)?;
    }
    Ok(())
}

fn apply_mcp_options(config: &mut Config, args: &SetupArgs) -> Result<()> {
    if let Some(mcp_servers) = &args.mcp_servers {
        config.mcp.mode = McpMode::Selected;
        config.mcp.servers = parse_names(mcp_servers)?;
    } else if args.all_mcp {
        config.mcp.mode = McpMode::All;
        config.mcp.servers.clear();
    } else if args.no_mcp {
        config.mcp.mode = McpMode::None;
        config.mcp.servers.clear();
    }
    Ok(())
}

fn apply_content_options(config: &mut Config, args: &SetupArgs) {
    if args.include_references {
        config.include_references = true;
    } else if args.exclude_references {
        config.include_references = false;
    }
    if args.allow_updates {
        config.allow_updates = true;
    } else if args.block_updates {
        config.allow_updates = false;
    }
}

fn apply_cursor_history_options(config: &mut Config, args: &SetupArgs) {
    if args.cursor_history {
        config.cursor_history.enabled = true;
    } else if args.no_cursor_history {
        config.cursor_history.enabled = false;
        config.cursor_history.refresh_qmd = false;
    }
    if args.skip_qmd {
        config.cursor_history.refresh_qmd = false;
    } else if args.refresh_qmd {
        config.cursor_history.enabled = true;
        config.cursor_history.refresh_qmd = true;
    }
}

fn run_sync(context: &AppContext, args: SyncArgs) -> Result<()> {
    let executable = std::env::current_exe()?;
    let report = sync_managed(
        &context.paths,
        &context.config_path,
        &executable,
        SyncOptions {
            dry_run: !args.yes,
            automation: args.automation,
        },
    )?;
    print!("{}", report.to_text());
    Ok(())
}

fn run_doctor(context: &AppContext) -> Result<()> {
    let executable = std::env::current_exe()?;
    let report = doctor_managed(&context.paths, &context.config_path, &executable);
    print!("{}", report.to_text());
    if !report.ok {
        std::process::exit(1);
    }
    Ok(())
}

fn run_init(args: PackArgs) -> Result<()> {
    let report = init_pack(&args.pack)?;
    print!("{}", report.to_text());
    Ok(())
}

fn run_status(context: &AppContext, args: StatusArgs) -> Result<()> {
    if let Some(pack) = args.pack {
        let targets = parse_targets(&args.targets)?;
        let plan = agent_sync::diff_pack(&context.paths, &pack, &targets)?;
        match args.format {
            OutputFormat::Text => print!("{}", format_diff(&plan)),
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&plan)?),
        }
    } else if context.config_path.exists() {
        let executable = std::env::current_exe()?;
        let background = inspect_background(context, &executable);
        let mut status =
            match status_managed_report(&context.paths, &context.config_path, &executable) {
                Ok(status) => status,
                Err(error) => {
                    if matches!(args.format, OutputFormat::Json) {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&CliStatusFailure {
                                configured: true,
                                healthy: false,
                                next_action: "doctor",
                                error: format!("{error:#}"),
                                background: &background.status,
                            })?
                        );
                    }
                    return Err(error).context("read managed agent-sync status");
                }
            };
        merge_background_health(&mut status, &background.status);
        match args.format {
            OutputFormat::Text => {
                print!("{}", status.to_text());
                print!("{}", background.status.to_text());
            }
            OutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&CliManagedStatus {
                    configured: true,
                    managed: &status,
                    background: &background.status,
                })?
            ),
        }
    } else {
        print_unconfigured_status(context, args.format)?;
    }
    Ok(())
}

fn print_unconfigured_status(context: &AppContext, format: OutputFormat) -> Result<()> {
    let inventory = discover(&context.paths)?;
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&CliUnconfiguredStatus {
                configured: false,
                healthy: false,
                next_action: "setup",
                inventory: &inventory,
            })?
        ),
        OutputFormat::Text => {
            print!("{}", inventory.to_text());
            println!("No managed sync is configured. Run `agent-sync setup` to preview one.");
        }
    }
    Ok(())
}

fn run_discover(context: &AppContext, args: DiscoverArgs) -> Result<()> {
    let inventory = discover(&context.paths)?;
    match args.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&inventory)?),
        OutputFormat::Text => print!("{}", inventory.to_text()),
    }
    Ok(())
}

fn run_export(context: &AppContext, args: ExportArgs) -> Result<()> {
    let report = export_pack(
        &context.paths,
        &args.pack,
        ExportOptions {
            source: args.source.into(),
            include_references: !args.portable_only,
            include_mcp: true,
            mcp_servers: args.mcp_servers,
        },
    )?;
    print!("{}", report.to_text());
    Ok(())
}

fn run_diff(context: &AppContext, args: PackTargetArgs) -> Result<()> {
    let targets = parse_targets(&args.targets)?;
    let plan = agent_sync::diff_pack(&context.paths, &args.pack, &targets)?;
    print!("{}", format_diff(&plan));
    Ok(())
}

fn run_apply(context: &AppContext, args: ApplyArgs) -> Result<()> {
    let targets = parse_targets(&args.targets)?;
    let report = apply_pack(
        &context.paths,
        &args.pack,
        &targets,
        ApplyOptions {
            dry_run: !args.yes,
            backup_root: None,
            allow_updates: true,
        },
    )?;
    print!("{}", report.to_text());
    Ok(())
}

fn run_verify(context: &AppContext, args: PackTargetArgs) -> Result<()> {
    let targets = parse_targets(&args.targets)?;
    let report = verify_pack(&context.paths, &args.pack, &targets)?;
    print!("{}", report.to_text());
    if !report.ok {
        std::process::exit(1);
    }
    Ok(())
}

fn run_cursor_history(context: &AppContext, command: CursorHistoryCommand) -> Result<()> {
    match command {
        CursorHistoryCommand::Install { executable, yes } => {
            let executable = executable.unwrap_or(std::env::current_exe()?);
            let report = install_cursor_history_hook(&context.paths, &executable, !yes)?;
            print!("{}", report.to_text());
        }
        CursorHistoryCommand::Export {
            output_dir,
            skip_qmd,
        } => {
            export_cursor_history_from_stdin(&context.paths, output_dir, !skip_qmd)?;
            println!("{{}}");
        }
        CursorHistoryCommand::RefreshQmd { output_dir } => {
            agent_sync::run_deferred_qmd_refresh(&context.paths, &output_dir)?;
        }
    }
    Ok(())
}

fn run_schedule(context: &AppContext, command: ScheduleCommand) -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("managed background schedules currently require macOS");
    }
    let executable = std::env::current_exe()?;
    let mut controller = SystemLaunchAgentController::for_current_user()?;
    let report = match command {
        ScheduleCommand::Status => {
            let spec = schedule_spec(context, executable)?;
            schedule_status(&spec, &mut controller)?
        }
        ScheduleCommand::Install { yes } => {
            ensure_schedule_ready(context)?;
            let spec = schedule_spec(context, executable)?;
            install_schedule(&spec, &mut controller, !yes)?
        }
        ScheduleCommand::Uninstall { yes } => {
            let spec = schedule_spec(context, executable)?;
            uninstall_schedule(&spec, &mut controller, !yes)?
        }
    };
    print!("{}", report.to_text());
    Ok(())
}

fn schedule_spec(context: &AppContext, executable: impl Into<PathBuf>) -> Result<ScheduleSpec> {
    let working_directory =
        std::env::current_dir().context("resolve schedule working directory")?;
    let path_arguments = [
        (
            "--config",
            absolute_schedule_path(&context.config_path, &working_directory),
        ),
        (
            "--home",
            absolute_schedule_path(&context.paths.home, &working_directory),
        ),
        (
            "--codex-home",
            absolute_schedule_path(&context.paths.codex_home, &working_directory),
        ),
        (
            "--claude-home",
            absolute_schedule_path(&context.paths.claude_home, &working_directory),
        ),
        (
            "--claude-config",
            absolute_schedule_path(&context.paths.claude_config, &working_directory),
        ),
        (
            "--cursor-home",
            absolute_schedule_path(&context.paths.cursor_home, &working_directory),
        ),
        (
            "--cursor-config",
            absolute_schedule_path(&context.paths.cursor_config, &working_directory),
        ),
        (
            "--agents-home",
            absolute_schedule_path(&context.paths.agents_home, &working_directory),
        ),
    ];
    let mut arguments = Vec::with_capacity(path_arguments.len() * 2);
    for (flag, path) in path_arguments {
        arguments.push(flag.to_string());
        arguments.push(
            path.to_str()
                .with_context(|| format!("{flag} path is not valid UTF-8: {}", path.display()))?
                .to_string(),
        );
    }
    let qmd = qmd_executable(&context.paths)
        .map(|path| absolute_schedule_path(&path, &working_directory));
    let environment_path = schedule_environment_path(qmd.as_deref());
    let home = absolute_schedule_path(&context.paths.home, &working_directory);
    let executable = absolute_schedule_path(&executable.into(), &working_directory);
    Ok(ScheduleSpec::new(home, executable)
        .with_global_arguments(arguments)
        .with_environment_path(environment_path))
}

fn absolute_schedule_path(path: &Path, working_directory: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_directory.join(path)
    }
}

fn schedule_environment_path(qmd: Option<&Path>) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(parent) = qmd.and_then(Path::parent) {
        directories.push(parent.to_path_buf());
    }
    for directory in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        let directory = PathBuf::from(directory);
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    directories
}

fn ensure_schedule_ready(context: &AppContext) -> Result<()> {
    if !context.config_path.exists() {
        anyhow::bail!("set up agent-sync before enabling its background schedule");
    }
    let executable = std::env::current_exe()?;
    let status = status_managed_report(&context.paths, &context.config_path, &executable)?;
    if status.last_success.is_none() {
        anyhow::bail!("run one successful `agent-sync sync --yes` before enabling the schedule");
    }
    if !status.healthy {
        anyhow::bail!("agent-sync needs attention; run `agent-sync doctor` before scheduling it");
    }
    Ok(())
}

impl From<SourceArg> for SourceSelection {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::All => SourceSelection::All,
            SourceArg::Codex => SourceSelection::Codex,
            SourceArg::Claude => SourceSelection::Claude,
            SourceArg::Cursor => SourceSelection::Cursor,
        }
    }
}

impl From<ManagedSourceArg> for CanonicalSource {
    fn from(value: ManagedSourceArg) -> Self {
        match value {
            ManagedSourceArg::Codex => CanonicalSource::Codex,
            ManagedSourceArg::Claude => CanonicalSource::Claude,
            ManagedSourceArg::Cursor => CanonicalSource::Cursor,
        }
    }
}

fn parse_targets(raw: &str) -> Result<Vec<AgentKind>> {
    let mut out = Vec::new();
    for part in raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let target = match part {
            "codex" => AgentKind::Codex,
            "claude" => AgentKind::Claude,
            "cursor" => AgentKind::Cursor,
            other => anyhow::bail!("unknown target `{other}`; expected codex, claude, or cursor"),
        };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    if out.is_empty() {
        anyhow::bail!("at least one target is required");
    }
    Ok(out)
}

fn parse_names(raw: &str) -> Result<Vec<String>> {
    let names = raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if names.is_empty() {
        anyhow::bail!("at least one MCP server name is required");
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_sources_include_only_agent_directories_found_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::for_test(temp.path());
        std::fs::create_dir_all(&paths.cursor_home).unwrap();

        assert_eq!(available_source_agents(&paths), vec![TuiAgent::Cursor]);

        std::fs::create_dir_all(&paths.codex_home).unwrap();
        assert_eq!(
            available_source_agents(&paths),
            vec![TuiAgent::Codex, TuiAgent::Cursor]
        );
    }

    #[test]
    fn schedule_path_keeps_a_custom_qmd_parent_and_system_directories() {
        let qmd = Path::new("/Users/test/.asdf/shims/qmd");

        let directories = schedule_environment_path(Some(qmd));

        assert_eq!(
            directories.first().unwrap(),
            Path::new("/Users/test/.asdf/shims")
        );
        assert!(directories.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(directories.contains(&PathBuf::from("/usr/local/bin")));
        assert!(directories.contains(&PathBuf::from("/usr/bin")));
        assert!(directories.contains(&PathBuf::from("/bin")));
        assert!(directories.contains(&PathBuf::from("/usr/sbin")));
        assert!(directories.contains(&PathBuf::from("/sbin")));
    }

    #[test]
    fn schedule_path_deduplicates_a_qmd_parent_already_in_the_system_path() {
        let directories = schedule_environment_path(Some(Path::new("/usr/local/bin/qmd")));

        assert_eq!(
            directories
                .iter()
                .filter(|path| path.as_path() == Path::new("/usr/local/bin"))
                .count(),
            1
        );
    }

    #[test]
    fn schedule_paths_make_relative_overrides_independent_of_launchd_working_directory() {
        let working_directory = Path::new("/Users/test/project");

        assert_eq!(
            absolute_schedule_path(Path::new("config/agent-sync.toml"), working_directory),
            PathBuf::from("/Users/test/project/config/agent-sync.toml")
        );
        assert_eq!(
            absolute_schedule_path(Path::new("/Users/test/.agent-sync.toml"), working_directory),
            PathBuf::from("/Users/test/.agent-sync.toml")
        );
    }
}

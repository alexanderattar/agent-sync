use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use agent_sync::{
    apply_pack, discover, doctor_managed, export_cursor_history_from_stdin, export_pack,
    format_diff, init_pack, install_cursor_history_hook, resolve_config_path, setup_managed,
    status_managed, sync_managed, verify_pack, AgentKind, AgentPaths, ApplyOptions,
    CanonicalSource, Config, ExportOptions, McpMode, SetupOptions, SourceSelection, SyncOptions,
};

#[derive(Debug, Parser)]
#[command(name = "agent-sync")]
#[command(about = "Synchronize personal agent tooling across local coding agents")]
#[command(version)]
struct Cli {
    #[command(flatten)]
    paths: PathArgs,

    #[command(subcommand)]
    command: Command,
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
    /// Configure a safe managed sync and install its natural-language skill
    Setup {
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
    },
    /// Preview or apply the configured sync, then verify and record it
    Sync {
        #[arg(long, help = "Apply the preview, verify it, and record the run")]
        yes: bool,

        #[arg(long, requires = "yes", help = "Use scheduled-task output conventions")]
        automation: bool,
    },
    /// Run a comprehensive read-only health and drift check
    Doctor,
    /// Initialize an empty low-level pack
    Init {
        #[arg(long)]
        pack: PathBuf,
    },
    /// Show managed health, inventory, or a low-level pack diff
    Status {
        #[arg(long)]
        pack: Option<PathBuf>,

        #[arg(long, default_value = "codex,claude")]
        targets: String,
    },
    /// List the agent resources visible on this machine
    Discover {
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Export resources to a low-level pack
    Export {
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
    },
    /// Compare a low-level pack with one or more agents
    Diff {
        #[arg(long)]
        pack: PathBuf,

        #[arg(long, default_value = "codex,claude")]
        targets: String,
    },
    /// Preview or apply a low-level pack
    Apply {
        #[arg(long)]
        pack: PathBuf,

        #[arg(long, default_value = "codex,claude")]
        targets: String,

        #[arg(
            long,
            help = "Actually write changes. Without this, apply prints the plan only."
        )]
        yes: bool,
    },
    /// Verify a low-level pack against one or more agents
    Verify {
        #[arg(long)]
        pack: PathBuf,

        #[arg(long, default_value = "codex,claude")]
        targets: String,
    },
    /// Install or run the Cursor chat export hook
    CursorHistory {
        #[command(subcommand)]
        command: CursorHistoryCommand,
    },
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
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ManagedSourceArg {
    Codex,
    Claude,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_override = cli.paths.config.clone();
    let paths = AgentPaths::from_optional(
        cli.paths.home,
        cli.paths.codex_home,
        cli.paths.claude_home,
        cli.paths.claude_config,
        cli.paths.cursor_home,
        cli.paths.cursor_config,
        cli.paths.agents_home,
    )?;
    let config_path = resolve_config_path(&paths.home, config_override.as_deref());

    match cli.command {
        Command::Setup {
            source,
            targets,
            mcp_servers,
            all_mcp,
            no_mcp,
            include_references,
            exclude_references,
            allow_updates,
            block_updates,
            cursor_history,
            no_cursor_history,
            skip_qmd,
            refresh_qmd,
            stale_after_hours,
            yes,
        } => {
            let mut config = if config_path.exists() {
                agent_sync::load_config(&config_path)?
            } else {
                Config::default()
            };
            if let Some(source) = source {
                config.source = source.into();
            }
            if let Some(targets) = targets {
                config.targets = parse_targets(&targets)?;
            }
            if let Some(mcp_servers) = mcp_servers {
                config.mcp.mode = McpMode::Selected;
                config.mcp.servers = parse_names(&mcp_servers)?;
            } else if all_mcp {
                config.mcp.mode = McpMode::All;
                config.mcp.servers.clear();
            } else if no_mcp {
                config.mcp.mode = McpMode::None;
                config.mcp.servers.clear();
            }
            if include_references {
                config.include_references = true;
            } else if exclude_references {
                config.include_references = false;
            }
            if allow_updates {
                config.allow_updates = true;
            } else if block_updates {
                config.allow_updates = false;
            }
            if cursor_history {
                if !config.cursor_history.enabled {
                    config.cursor_history.refresh_qmd = true;
                }
                config.cursor_history.enabled = true;
            } else if no_cursor_history {
                config.cursor_history.enabled = false;
                config.cursor_history.refresh_qmd = false;
            }
            if skip_qmd {
                config.cursor_history.refresh_qmd = false;
            } else if refresh_qmd {
                config.cursor_history.enabled = true;
                config.cursor_history.refresh_qmd = true;
            }
            if let Some(stale_after_hours) = stale_after_hours {
                config.health.stale_after_hours = stale_after_hours;
            }
            let report = setup_managed(
                &paths,
                &config_path,
                &config,
                SetupOptions { dry_run: !yes },
            )?;
            print!("{}", report.to_text());
        }
        Command::Sync { yes, automation } => {
            let executable = std::env::current_exe()?;
            let report = sync_managed(
                &paths,
                &config_path,
                &executable,
                SyncOptions {
                    dry_run: !yes,
                    automation,
                },
            )?;
            print!("{}", report.to_text());
        }
        Command::Doctor => {
            let executable = std::env::current_exe()?;
            let report = doctor_managed(&paths, &config_path, &executable);
            print!("{}", report.to_text());
            if !report.ok {
                std::process::exit(1);
            }
        }
        Command::Init { pack } => {
            let report = init_pack(&pack)?;
            print!("{}", report.to_text());
        }
        Command::Status { pack, targets } => {
            if let Some(pack) = pack {
                let targets = parse_targets(&targets)?;
                let plan = agent_sync::diff_pack(&paths, &pack, &targets)?;
                print!("{}", format_diff(&plan));
            } else if config_path.exists() {
                let executable = std::env::current_exe()?;
                print!("{}", status_managed(&paths, &config_path, &executable)?);
            } else {
                let inventory = discover(&paths)?;
                print!("{}", inventory.to_text());
                println!("No managed sync is configured. Run `agent-sync setup` to preview one.");
            }
        }
        Command::Discover { format } => {
            let inventory = discover(&paths)?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&inventory)?),
                OutputFormat::Text => print!("{}", inventory.to_text()),
            }
        }
        Command::Export {
            pack,
            source,
            portable_only,
            mcp_servers,
        } => {
            let report = export_pack(
                &paths,
                &pack,
                ExportOptions {
                    source: source.into(),
                    include_references: !portable_only,
                    include_mcp: true,
                    mcp_servers,
                },
            )?;
            print!("{}", report.to_text());
        }
        Command::Diff { pack, targets } => {
            let targets = parse_targets(&targets)?;
            let plan = agent_sync::diff_pack(&paths, &pack, &targets)?;
            print!("{}", format_diff(&plan));
        }
        Command::Apply { pack, targets, yes } => {
            let targets = parse_targets(&targets)?;
            let report = apply_pack(
                &paths,
                &pack,
                &targets,
                ApplyOptions {
                    dry_run: !yes,
                    backup_root: None,
                    allow_updates: true,
                },
            )?;
            print!("{}", report.to_text());
        }
        Command::Verify { pack, targets } => {
            let targets = parse_targets(&targets)?;
            let report = verify_pack(&paths, &pack, &targets)?;
            print!("{}", report.to_text());
            if !report.ok {
                std::process::exit(1);
            }
        }
        Command::CursorHistory { command } => match command {
            CursorHistoryCommand::Install { executable, yes } => {
                let executable = executable.unwrap_or(std::env::current_exe()?);
                let report = install_cursor_history_hook(&paths, &executable, !yes)?;
                print!("{}", report.to_text());
            }
            CursorHistoryCommand::Export {
                output_dir,
                skip_qmd,
            } => {
                export_cursor_history_from_stdin(&paths, output_dir, !skip_qmd)?;
                println!("{{}}");
            }
        },
    }

    Ok(())
}

impl From<SourceArg> for SourceSelection {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::All => SourceSelection::All,
            SourceArg::Codex => SourceSelection::Codex,
            SourceArg::Claude => SourceSelection::Claude,
        }
    }
}

impl From<ManagedSourceArg> for CanonicalSource {
    fn from(value: ManagedSourceArg) -> Self {
        match value {
            ManagedSourceArg::Codex => CanonicalSource::Codex,
            ManagedSourceArg::Claude => CanonicalSource::Claude,
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

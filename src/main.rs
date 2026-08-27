use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use agent_sync::{
    apply_pack, discover, export_cursor_history_from_stdin, export_pack, format_diff, init_pack,
    install_cursor_history_hook, verify_pack, AgentKind, AgentPaths, ApplyOptions, ExportOptions,
    SourceSelection,
};

#[derive(Debug, Parser)]
#[command(name = "agent-sync")]
#[command(about = "Synchronize personal agent tooling across local coding agents")]
struct Cli {
    #[command(flatten)]
    paths: PathArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Args)]
struct PathArgs {
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
    Init {
        #[arg(long)]
        pack: PathBuf,
    },
    Status {
        #[arg(long)]
        pack: Option<PathBuf>,

        #[arg(long, default_value = "codex,claude")]
        targets: String,
    },
    Discover {
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
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
    Diff {
        #[arg(long)]
        pack: PathBuf,

        #[arg(long, default_value = "codex,claude")]
        targets: String,
    },
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
    Verify {
        #[arg(long)]
        pack: PathBuf,

        #[arg(long, default_value = "codex,claude")]
        targets: String,
    },
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AgentPaths::from_optional(
        cli.paths.home,
        cli.paths.codex_home,
        cli.paths.claude_home,
        cli.paths.claude_config,
        cli.paths.cursor_home,
        cli.paths.cursor_config,
        cli.paths.agents_home,
    )?;

    match cli.command {
        Command::Init { pack } => {
            let report = init_pack(&pack)?;
            print!("{}", report.to_text());
        }
        Command::Status { pack, targets } => {
            if let Some(pack) = pack {
                let targets = parse_targets(&targets)?;
                let plan = agent_sync::diff_pack(&paths, &pack, &targets)?;
                print!("{}", format_diff(&plan));
            } else {
                let inventory = discover(&paths)?;
                print!("{}", inventory.to_text());
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
            let report = apply_pack(&paths, &pack, &targets, ApplyOptions { dry_run: !yes })?;
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

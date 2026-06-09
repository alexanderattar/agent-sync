use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use agent_sync::{
    apply_pack, discover, export_pack, format_diff, init_pack, verify_pack, AgentKind, AgentPaths,
    ApplyOptions, ExportOptions, SourceSelection,
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
        Command::Export { pack, source } => {
            let report = export_pack(
                &paths,
                &pack,
                ExportOptions {
                    source: source.into(),
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
            other => anyhow::bail!("unknown target `{other}`; expected codex or claude"),
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

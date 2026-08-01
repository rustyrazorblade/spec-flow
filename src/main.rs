//! The `spec-flow` binary: a thin `clap` CLI over the [`spec_flow`]
//! library.
//!
//! Per the style guide's error-handling split, this is the only place
//! in the crate that uses `anyhow` — everything in the library returns
//! its own hand-written error types, and this binary's `main` reports
//! them (formatting the whole chain) and picks the process exit code.
//!
//! Two subcommands ship today, matching spec §14.1's packaging plan
//! (one binary, `serve` + `init`):
//!
//! - `spec-flow serve` — run the daemon (MCP over HTTP/SSE, loopback,
//!   §2.5). **Not implemented yet** (§14 steps 2+); this only logs that
//!   fact and exits.
//! - `spec-flow init` — register the current repo with the daemon (§6,
//!   §11). Wired to [`spec_flow::init`], which takes its
//!   [`spec_flow::Vcs`] implementation as an argument; constructing the
//!   real [`spec_flow::ShellVcs`] from the configured binary paths
//!   (§8.5) is this binary's job.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// A standalone, project-agnostic MCP daemon for AI-agent software
/// delivery.
#[derive(Debug, Parser)]
#[command(name = "spec-flow", version, about)]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon (MCP over HTTP/SSE, loopback). Not implemented
    /// yet.
    Serve(ServeArgs),

    /// Register this repo with the daemon and scaffold its
    /// `.spec-flow/` files.
    Init(InitArgs),
}

/// Arguments for `spec-flow serve`.
#[derive(Debug, Parser)]
struct ServeArgs {}

/// Arguments for `spec-flow init`.
#[derive(Debug, Parser)]
struct InitArgs {
    /// Override the detected GitHub repo slug (`owner/repo`).
    #[arg(long)]
    repo: Option<String>,

    /// Override the project's container directory (defaults to the
    /// current directory's parent, per §10.1's sibling-worktree
    /// layout).
    #[arg(long)]
    project_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "spec_flow=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve(_args) => {
            // Task #14 steps 2+: the process spawner, phase engine, and
            // MCP-over-HTTP/SSE surface all land before `serve` does
            // anything real. Logging (not `println!`) per the style
            // guide's observability rule, even for a stub.
            tracing::info!("spec-flow serve is not yet implemented");
            Ok(())
        }
        Command::Init(args) => {
            // Honor a pinned `binaries.git`/`binaries.gh` (§8.5) when the
            // global config already exists; on a first `init` it does not
            // yet, so fall back to plain `PATH` lookup. A config that
            // exists but cannot be read is not swallowed here — `init`
            // loads it again itself and reports the failure.
            let binaries = spec_flow::global_config_path()
                .and_then(|path| spec_flow::load_global_config(&path))
                .map(|config| config.binaries)
                .unwrap_or_default();
            let vcs = spec_flow::ShellVcs::new(binaries.git, binaries.gh);

            spec_flow::init(
                &vcs,
                spec_flow::InitOptions {
                    repo: args.repo,
                    project_dir: args.project_dir,
                },
            )?;
            Ok(())
        }
    }
}

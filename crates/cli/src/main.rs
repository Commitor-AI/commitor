use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod analysis;
mod auth;
mod commit;
mod config;
mod engine;
mod heuristics;
mod scan;

use engine::update;

/// Catch unrelated changes before they get buried
#[derive(Parser)]
#[command(name = "commitor", version, about = "Catch unrelated changes before they get buried")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate an API key against the backend and store it locally
    Login {
        /// API key from https://commitor.dev/dashboard
        #[arg(long)]
        key: String,
    },
    /// Delete the locally stored credentials
    Logout,
    /// Show the account and plan behind the stored API key
    Whoami,
    /// Analyze the working diff for unrelated changes (read-only)
    Scan {
        /// Scan unstaged changes instead of staged ones
        #[arg(long)]
        all: bool,
        /// Only run local heuristics; never call the backend
        #[arg(long)]
        offline: bool,
        /// Exit non-zero when the commit looks mixed (CI / pre-commit)
        #[arg(long)]
        strict: bool,
        /// Print machine-readable JSON instead of a formatted report
        #[arg(long)]
        json: bool,
    },
    /// Analyze the working diff and create the approved git commits
    Commit {
        /// Plan commits from unstaged changes instead of staged ones
        #[arg(long)]
        all: bool,
        /// Skip the AI entirely — plan from local heuristics only
        /// (no account or backend needed)
        #[arg(long)]
        offline: bool,
    },
    /// Update commitor to the latest release
    Update,
}

fn main() -> ExitCode {
    // A previous Windows self-update may have left `<exe>.old` behind.
    update::cleanup_stale_old_binary();

    let cli = Cli::parse();

    // Server-facing commands only run when the CLI is current; pure
    // info commands (`version`, help) and the self-updater stay
    // available regardless.
    let result: Result<ExitCode, Option<anyhow::Error>> =
        if is_server_facing(&cli.command) {
            match enforce_up_to_date() {
                Ok(None) => execute(cli.command),
                Ok(Some(latest)) => {
                    eprintln!("error: commitor {latest} is available and must be installed first");
                    eprintln!();
                    eprintln!("Run `commitor update` to install it, then retry your command.");
                    eprintln!("Set COMMITOR_ALLOW_OUTDATED=1 to bypass this check at your own risk.");
                    Err(None)
                }
                Err(err) => Err(Some(err)),
            }
        } else {
            execute(cli.command)
        };

    // `Err(None)` means the failure was already reported to stderr.
    match result {
        Ok(code) => code,
        Err(Some(err)) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
        Err(None) => ExitCode::FAILURE,
    }
}

fn is_server_facing(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Login { .. }
            | Commands::Logout
            | Commands::Whoami
            | Commands::Scan { .. }
            | Commands::Commit { .. }
    )
}

fn execute(command: Commands) -> Result<ExitCode, Option<anyhow::Error>> {
    match command {
        Commands::Login { key } => auth::login(&key).map(|_| ExitCode::SUCCESS).map_err(Some),
        Commands::Logout => auth::logout().map(|_| ExitCode::SUCCESS).map_err(Some),
        Commands::Whoami => auth::whoami().map(|_| ExitCode::SUCCESS).map_err(Some),
        Commands::Scan {
            all,
            offline,
            strict,
            json,
        } => scan::run(scan::ScanFlags {
            all,
            offline,
            strict,
            json,
        }).map_err(Some),
        Commands::Commit { all, offline } => {
            commit::run(commit::CommitFlags { all, offline }).map_err(Some)
        }
        Commands::Update => run_update().map(|_| ExitCode::SUCCESS).map_err(Some),
    }
}

/// Escape hatch for users the updater cannot serve (no matching asset,
/// broken network on every attempt, ...).
fn allow_outdated() -> bool {
    std::env::var("COMMITOR_ALLOW_OUTDATED")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Make sure no newer release is pending before the caller proceeds.
///
/// Returns `Ok(Some(latest))` when an update must be installed first,
/// `Ok(None)` when the CLI is up to date. The check itself:
///
/// 1. reads the local pending-update marker (instant), then
/// 2. refreshes it against GitHub at most once a day.
///
/// Network or API failures never lock the CLI out; they just defer
/// enforcement to a later invocation. `COMMITOR_ALLOW_OUTDATED=1`
/// skips the gate entirely.
fn enforce_up_to_date() -> Result<Option<String>> {
    if allow_outdated() {
        return Ok(None);
    }

    if let Some(latest) = update::pending_version().context(
        "failed to read the pending-update state — set COMMITOR_ALLOW_OUTDATED=1 to bypass",
    )? {
        return Ok(Some(latest));
    }

    if !matches!(update::update_check_due(), Ok(true)) {
        return Ok(None);
    }

    let checked = || -> Result<Option<String>> {
        let release = update::check_latest_release()?;
        update::mark_update_checked()?;
        if update::is_newer(&release.tag_name, update::current_version()) {
            update::record_pending_version(&release.tag_name)?;
            Ok(Some(release.tag_name))
        } else {
            Ok(None)
        }
    }();

    match checked {
        Ok(found) => Ok(found),
        // Fail open: a flaky network should not brick the CLI.
        Err(_) => Ok(None),
    }
}

fn run_update() -> Result<()> {
    let current = update::current_version();

    println!("Checking for updates…");
    let release = update::check_latest_release()
        .context("failed to check for the latest release — check your internet connection")?;

    if !update::is_newer(&release.tag_name, current) {
        println!("You're already on the latest version (v{current}).");
        // A manual reinstall may have left a stale marker behind.
        let _ = update::clear_pending_version();
        return Ok(());
    }

    println!(
        "New version available: {} (you have v{current})",
        release.tag_name
    );

    let asset = update::find_matching_asset(&release)?;
    if !confirm(&format!(
        "Download and install {} ({})?",
        release.tag_name, asset.name
    ))? {
        println!("Update aborted.");
        return Ok(());
    }

    println!("Downloading {}…", asset.name);
    update::download_and_replace_binary(&asset.browser_download_url)?;
    let _ = update::clear_pending_version();

    println!(
        "Installed {}. Run `commitor --version` in a new shell to confirm.",
        release.tag_name
    );
    Ok(())
}

fn confirm(question: &str) -> io::Result<bool> {
    print!("{question} [y/N] ");
    io::stdout().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

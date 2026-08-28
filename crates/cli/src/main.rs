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
    /// Sign in to Commitor: opens your browser to the web app, or pass
    /// --key with an API key copied from the dashboard
    Login {
        /// API key from the dashboard (skips the browser flow)
        #[arg(long)]
        key: Option<String>,
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
        /// Interactively choose the branch to commit to: lists local
        /// branches (marking the current one) plus a "new branch"
        /// option, then switches to the choice before committing. The
        /// new-branch prompt suggests a name from the diff (Tab to
        /// accept, or type your own; Ctrl-C to skip)
        #[arg(short = 'b')]
        branch: bool,
    },
    /// Update commitor to the latest release
    Update,
}

fn main() -> ExitCode {
    // A previous Windows self-update may have left `<exe>.old` behind.
    update::cleanup_stale_old_binary();

    let cli = Cli::parse();

    // Remind (but never block) users running an older build before
    // server-facing commands. Previous versions keep working against the
    // API — updating is always optional. Pure info commands
    // (`version`, help) and the self-updater stay unaffected.
    let result: Result<ExitCode, Option<anyhow::Error>> =
        if is_server_facing(&cli.command) {
            notify_update_available();
            execute(cli.command)
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
        Commands::Login { key } => match key {
            Some(k) => auth::login(&k),
            None => auth::login_interactive(),
        }
        .map(|_| ExitCode::SUCCESS)
        .map_err(Some),
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
        Commands::Commit { all, offline, branch } => {
            commit::run(commit::CommitFlags { all, offline, branch }).map_err(Some)
        }
        Commands::Update => run_update().map(|_| ExitCode::SUCCESS).map_err(Some),
    }
}

/// Escape hatch for users who would rather not be reminded about
/// available updates (no matching asset, prefer to stay on a pinned
/// version, ...). When set, no update notice is printed.
fn allow_outdated() -> bool {
    std::env::var("COMMITOR_ALLOW_OUTDATED")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Print a non-fatal notice when a newer release exists, then return so
/// the command proceeds regardless. Users are never required to update
/// — older versions keep working against the API.
///
/// The check itself:
///
/// 1. reads the local pending-update marker (instant), then
/// 2. refreshes it against GitHub at most once a day.
///
/// Network or API failures never interrupt the user; they just skip the
/// notice. `COMMITOR_ALLOW_OUTDATED=1` suppresses the notice entirely.
fn notify_update_available() {
    if allow_outdated() {
        return;
    }

    // Fast path: a previously recorded newer release.
    if let Ok(Some(latest)) = update::pending_version() {
        print_update_notice(&latest);
        return;
    }

    if !matches!(update::update_check_due(), Ok(true)) {
        return;
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
        // Fail open: a flaky network should not bother the user.
        Ok(Some(latest)) => print_update_notice(&latest),
        _ => {}
    }
}

fn print_update_notice(latest: &str) {
    eprintln!(
        "note: commitor {} is available — run `commitor update` to upgrade (current: v{}).",
        latest,
        update::current_version()
    );
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

use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::{Context, Result};

use engine::update;

const USAGE: &str = "commitor — catch unrelated changes before they get buried

Usage: commitor <COMMAND>

Commands:
  update     Update commitor to the latest release
  version    Print the installed version (also: --version, -V)

Options:
  -h, --help  Show this help";

fn main() -> ExitCode {
    // A previous Windows self-update may have left `<exe>.old` behind.
    update::cleanup_stale_old_binary();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);

    let result: Result<(), Option<anyhow::Error>> =
        if let Some(cmd) = command.filter(|cmd| !gate_exempt(Some(cmd))) {
            run_gated_command(cmd)
        } else {
            match command {
                None | Some("-h" | "--help" | "help") => {
                    println!("{USAGE}");
                    Ok(())
                }
                Some("update") => run_update().map_err(Some),
                Some("--version" | "-V" | "version") => {
                    println!("commitor v{}", update::current_version());
                    Ok(())
                }
                _ => unreachable!("gate_exempt() covers every arm above"),
            }
        };

    // `Err(None)` means the failure was already reported to stderr.
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Some(err)) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
        Err(None) => ExitCode::FAILURE,
    }
}

/// Commands that stay available while an update is pending: the
/// self-updater itself plus harmless informational commands.
fn gate_exempt(command: Option<&str>) -> bool {
    matches!(
        command,
        None | Some("-h" | "--help" | "help")
            | Some("update")
            | Some("--version" | "-V" | "version")
    )
}

/// Escape hatch for users the updater cannot serve (no matching asset,
/// broken network on every attempt, ...).
fn allow_outdated() -> bool {
    std::env::var("COMMITOR_ALLOW_OUTDATED")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Run a real (non-exempt) command.
///
/// Nothing executes until [`enforce_up_to_date`] confirms the CLI is
/// current: once an update is known to exist, work is refused until
/// the user runs `commitor update`.
fn run_gated_command(command: &str) -> Result<(), Option<anyhow::Error>> {
    match enforce_up_to_date() {
        Ok(None) => {}
        Ok(Some(latest)) => {
            eprintln!("error: commitor {latest} is available and must be installed first");
            eprintln!();
            eprintln!("Run `commitor update` to install it, then retry your command.");
            eprintln!("Set COMMITOR_ALLOW_OUTDATED=1 to bypass this check at your own risk.");
            return Err(None);
        }
        Err(err) => return Err(Some(err)),
    }

    // Future work commands plug in here — they are guaranteed to be
    // up to date by the gate above.
    eprintln!("error: unknown command `{command}`\n");
    eprintln!("{USAGE}");
    Err(None)
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

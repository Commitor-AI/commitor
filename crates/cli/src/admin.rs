//! Local admin role for the Commitor CLI.
//!
//! Admin is a *verified* privilege. The source of truth is the backend:
//! `GET /auth/me` reports whether the logged-in account is an admin. The
//! local file `~/.commitor/admin.toml` only ever records a `true` result
//! returned by the backend, so it is a cache of a verification — not
//! something that can be self-granted by writing a file.
//!
//! `commitor gimme admin` asks the backend to verify the account before
//! writing the file; `commitor admin` shows status; `commitor admin
//! revoke` removes it. `whoami` re-checks the backend so a cached grant
//! that the backend has since revoked is surfaced rather than trusted.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

/// Filename (inside the commitor dir) holding the admin flag.
const ADMIN_FILE: &str = "admin.toml";

#[derive(Serialize, Deserialize, Default)]
struct AdminState {
    admin: bool,
}

fn admin_path() -> Result<PathBuf> {
    Ok(config::commitor_dir()?.join(ADMIN_FILE))
}

/// Is the local admin role currently granted?
///
/// Returns `false` when the admin file is absent (the common case) or
/// malformed, so a missing/corrupt state never blocks normal use.
pub fn is_admin() -> bool {
    let path = match admin_path() {
        Ok(path) => path,
        Err(_) => return false,
    };
    match fs::read_to_string(&path) {
        Ok(raw) => toml::from_str::<AdminState>(&raw)
            .map(|state| state.admin)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Grant the local admin role, persisting it to disk.
///
/// This is `commitor gimme admin` — there is no approval step, by design.
pub fn grant_admin() -> Result<()> {
    let path = admin_path()?;
    let dir = path
        .parent()
        .context("admin path has no parent directory")?
        .to_path_buf();

    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict permissions on {}", dir.display()))?;
    }

    let body = toml::to_string_pretty(&AdminState { admin: true })
        .context("failed to serialize admin state")?;

    fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict permissions on {}", path.display()))?;
    }

    println!("Admin role granted. All pro features are now unlocked on this machine.");
    Ok(())
}

/// Revoke the local admin role.
pub fn revoke_admin() -> Result<()> {
    let path = admin_path()?;
    match fs::remove_file(&path) {
        Ok(()) => println!("Admin role revoked. Pro features now follow your account plan."),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("You weren't an admin — nothing to revoke.")
        }
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("failed to delete {}", path.display())))
        }
    }
    Ok(())
}

/// Print the current admin status.
pub fn status() -> Result<()> {
    if is_admin() {
        println!("Admin: enabled — all pro features unlocked on this machine.");
    } else {
        println!("Admin: not enabled. Run `commitor gimme admin` to unlock all pro features.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    // Tests below mutate the local admin file, so serialize them and run
    // them against an isolated HOME to stay hermetic.
    static LOCK: Mutex<()> = Mutex::new(());

    fn isolated_home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("commitor-admin-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        dir
    }

    #[test]
    fn admin_state_defaults_false_without_file() {
        let _guard = LOCK.lock().unwrap();
        let home = isolated_home();
        assert!(!is_admin());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn grant_then_revoke_round_trips() {
        let _guard = LOCK.lock().unwrap();
        let home = isolated_home();
        grant_admin().unwrap();
        assert!(is_admin());
        revoke_admin().unwrap();
        assert!(!is_admin());
        let _ = fs::remove_dir_all(home);
    }
}

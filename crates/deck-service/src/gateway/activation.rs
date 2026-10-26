//! Notification activation without a window: the gateway writes a private one-use ticket and
//! starts `omp-deck --activate <ticket-file>`; that process [`consume`]s the ticket (only from the
//! identity's own private ticket directory) and asks the gateway to open the session, provided the
//! ticket's epoch is the connected gateway's.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{Profile, StartupContext, private_dir, write_private};
use crate::controller::{ServiceEpoch, SessionTarget};

/// Ticket directory name inside the identity's state directory.
pub const DIR: &str = "activation";
/// Tickets older than this are removed when the next one is written (their GUI never came).
const STALE: Duration = Duration::from_secs(60 * 60);
/// Random bytes of a nonce (hex in the file name).
const NONCE_BYTES: usize = 16;

/// One click on a desktop notification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationTicket {
    /// The gateway instance that wrote it (the target's epoch).
    pub epoch: ServiceEpoch,
    pub target: SessionTarget,
    /// The desktop's activation token (Wayland `xdg-activation`), when the notification server
    /// sent one.
    pub token: Option<String>,
    /// Random; also the file name.
    pub nonce: String,
}

/// `<state dir>/activation`.
pub fn ticket_dir(profile: &Profile) -> PathBuf {
    profile.state_dir().join(DIR)
}

/// Writes a ticket for `target` (owner-only, in the private ticket directory); returns its path.
pub fn write_ticket(
    context: &StartupContext,
    target: &SessionTarget,
    token: Option<&str>,
) -> io::Result<PathBuf> {
    let dir = ticket_dir(&context.profile);
    private_dir(&dir)?;
    prune(&dir);
    let mut bytes = [0u8; NONCE_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(format!("no random source: {e}")))?;
    let nonce: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let ticket = ActivationTicket {
        epoch: target.epoch,
        target: target.clone(),
        token: token.map(str::to_owned),
        nonce,
    };
    let path = dir.join(format!("{}.json", ticket.nonce));
    let json = serde_json::to_vec(&ticket).map_err(io::Error::other)?;
    write_private(&path, &json)?;
    Ok(path)
}

/// Removes tickets nobody consumed.
fn prune(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE);
        if old && ticket_name(&e.file_name().to_string_lossy()) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// `<32 lowercase hex>.json`.
fn ticket_name(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(|stem| {
        stem.len() == NONCE_BYTES * 2
            && stem
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn refuse(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why.into())
}

/// Reads and deletes the ticket at `path`. Refused (and left alone) unless it is a regular,
/// owner-only ticket file of this user directly inside `profile`'s private ticket directory:
/// an arbitrary command-line path is never read as a ticket nor deleted.
pub fn consume(path: &Path, profile: &Profile) -> io::Result<ActivationTicket> {
    let dir = ticket_dir(profile);
    let dir = fs::canonicalize(&dir).map_err(|e| {
        refuse(format!(
            "no activation ticket directory ({}): {e}",
            dir.display()
        ))
    })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| ticket_name(n))
        .ok_or_else(|| refuse(format!("{} is not an activation ticket", path.display())))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let parent = fs::canonicalize(&parent)?;
    if parent != dir {
        return Err(refuse(format!(
            "{} is not in the activation ticket directory",
            path.display()
        )));
    }
    check_private(&dir, true)?;
    let file = dir.join(name);
    check_private(&file, false)?;
    let bytes = fs::read(&file)?;
    let ticket: ActivationTicket = serde_json::from_slice(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("activation ticket: {e}"),
        )
    })?;
    // One use, whatever its content says.
    fs::remove_file(&file)?;
    if name.strip_suffix(".json") != Some(ticket.nonce.as_str()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activation ticket does not match its file",
        ));
    }
    if ticket.epoch != ticket.target.epoch {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activation ticket epoch does not match its session",
        ));
    }
    Ok(ticket)
}

/// A real (not linked) directory or regular file owned by this user, without group/other
/// access (Unix; Windows relies on the private directory's ACL).
fn check_private(path: &Path, dir: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let kind_ok = if dir {
        meta.file_type().is_dir()
    } else {
        meta.file_type().is_file()
    };
    if !kind_ok || deck_core::platform::is_link(&meta) {
        return Err(refuse(format!(
            "{} is not a private ticket",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        if meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(refuse(format!(
                "{} is not owned by this user or not private",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::Program;

    fn context(root: &Path) -> StartupContext {
        StartupContext {
            profile: Profile {
                host_namespace: "omp-deck-test-activation".into(),
                settings: root.join("state.json"),
                sessions_root: root.join("sessions"),
            },
            env: Vec::new(),
            gateway: Program {
                program: PathBuf::from("/nonexistent/omp-deck-gateway"),
                args: Vec::new(),
            },
            gui: None,
            host_override: None,
            omp: None,
        }
    }

    fn target() -> SessionTarget {
        SessionTarget {
            epoch: ServiceEpoch([7; 16]),
            key: serde_json::from_str("\"direct:test-1\"").expect("key"),
            generation: 3,
        }
    }

    #[test]
    fn a_ticket_is_private_one_use_and_round_trips() {
        let root = tempfile::tempdir().expect("temp dir");
        let ctx = context(root.path());
        let path = write_ticket(&ctx, &target(), Some("tok")).expect("write");
        assert!(path.starts_with(ticket_dir(&ctx.profile)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o077, 0, "ticket is owner-only");
        }
        let t = consume(&path, &ctx.profile).expect("consume");
        assert_eq!(t.target, target());
        assert_eq!(t.epoch, target().epoch);
        assert_eq!(t.token.as_deref(), Some("tok"));
        assert!(!path.exists(), "consumed tickets are deleted");
        assert!(consume(&path, &ctx.profile).is_err(), "one use");
    }

    #[test]
    fn foreign_paths_are_refused_and_never_deleted() {
        let root = tempfile::tempdir().expect("temp dir");
        let ctx = context(root.path());
        let real = write_ticket(&ctx, &target(), None).expect("write");
        let name = real.file_name().expect("name").to_owned();

        // Same file name, another directory.
        let elsewhere = root.path().join(&name);
        fs::copy(&real, &elsewhere).expect("copy");
        assert!(consume(&elsewhere, &ctx.profile).is_err());
        assert!(elsewhere.exists(), "a foreign file is left alone");

        // Not a ticket name, inside the directory.
        let odd = ticket_dir(&ctx.profile).join("notes.json");
        fs::write(&odd, b"{}").expect("write");
        assert!(consume(&odd, &ctx.profile).is_err());
        assert!(odd.exists());

        // Another profile's directory.
        let other = context(&root.path().join("other"));
        assert!(consume(&real, &other.profile).is_err());
        assert!(real.exists());

        #[cfg(unix)]
        {
            // A link to the ticket inside the directory is not a ticket.
            let link = ticket_dir(&ctx.profile).join(format!("{}.json", "ab".repeat(NONCE_BYTES)));
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            assert!(consume(&link, &ctx.profile).is_err());
            assert!(real.exists());
        }
        assert!(consume(&real, &ctx.profile).is_ok());
    }
}

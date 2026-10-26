//! The user systemd unit that supervises the gateway.
//!
//! `KillMode=process` is mandatory: the supervisor shares its cgroup with every session host and
//! omp child ever spawned under it (`setsid` does not leave a cgroup), so stopping the unit must
//! signal the supervisor only. There is no finite stop timeout and no `SIGKILL` either, so a
//! stop never cuts an accepted job short.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;

/// Largest unit file read back while checking ownership.
const MAX_UNIT_BYTES: u64 = 64 * 1024;

pub(super) fn user_unit_dir() -> Result<PathBuf, AutostartError> {
    dirs::config_dir()
        .map(|d| d.join("systemd").join("user"))
        .ok_or_else(|| AutostartError::Unavailable("no user configuration directory".into()))
}

pub(super) fn unit_name(namespace: &str) -> String {
    format!("omp-deck-gateway-{namespace}.service")
}

/// The first line of every unit OmpDeck writes; ownership is decided by it alone.
fn marker(namespace: &str) -> String {
    format!(
        "# Managed by OmpDeck: gateway supervisor for namespace {namespace}. Rewritten on registration."
    )
}

/// Double-quoted systemd word: `\`, `"`, and the `%` / `$` expansions are escaped.
fn word(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(super) fn render(startup: &Startup) -> Result<String, AutostartError> {
    let program = startup.validate(super::unix_absolute)?;
    // An unquoted `;` word separates commands in ExecStart and has no safe literal form here.
    if startup.args.iter().any(|a| a == ";") {
        return Err(invalid(
            "a lone ';' argument cannot be expressed in a unit".into(),
        ));
    }
    let mut exec = word(program);
    for arg in &startup.args {
        exec.push(' ');
        exec.push_str(&word(arg));
    }
    Ok(format!(
        "{marker}\n\
         [Unit]\n\
         Description=OmpDeck gateway supervisor ({ns})\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         KillMode=process\n\
         SendSIGKILL=no\n\
         TimeoutStopSec=infinity\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        marker = marker(&startup.namespace),
        ns = startup.namespace,
    ))
}

/// The unit's text when it exists and is OmpDeck's; `None` when absent.
fn read_owned(path: &Path, namespace: &str) -> Result<Option<String>, AutostartError> {
    let conflict = || AutostartError::Conflict {
        name: path.display().to_string(),
    };
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(path, e)),
    };
    if !meta.is_file() || meta.len() > MAX_UNIT_BYTES {
        return Err(conflict());
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Err(conflict()),
        Err(e) => return Err(io_error(path, e)),
    };
    if text.lines().next() == Some(marker(namespace).as_str()) {
        Ok(Some(text))
    } else {
        Err(conflict())
    }
}

fn namespace_of(name: &str) -> Option<&str> {
    name.strip_prefix("omp-deck-gateway-")?
        .strip_suffix(".service")
        .filter(|ns| deck_host::valid_namespace(ns))
}

fn ctl(run: Runner<'_>, args: &[&str]) -> Result<Out, AutostartError> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    run("systemctl", &full)
}

fn ctl_ok(run: Runner<'_>, args: &[&str]) -> Result<(), AutostartError> {
    let out = ctl(run, args)?;
    if out.ok() {
        Ok(())
    } else {
        let mut full = vec!["--user"];
        full.extend_from_slice(args);
        Err(command_error("systemctl", &full, &out))
    }
}

/// Fails with `Unavailable` unless a user manager answers (no D-Bus session, no lingering
/// user instance, container without systemd…): background operation is never claimed then.
fn ensure_manager(run: Runner<'_>) -> Result<(), AutostartError> {
    let out = ctl(run, &["show-environment"])?;
    if out.ok() {
        Ok(())
    } else {
        Err(AutostartError::Unavailable(format!(
            "no systemd user manager: {}",
            out.detail()
        )))
    }
}

pub(super) fn register(
    dir: &Path,
    startup: &Startup,
    run: Runner<'_>,
) -> Result<Registration, AutostartError> {
    let text = render(startup)?;
    let unit = unit_name(&startup.namespace);
    let path = dir.join(&unit);
    ensure_manager(run)?;
    let previous = read_owned(&path, &startup.namespace)?;
    let changed = previous.as_deref() != Some(text.as_str());
    if changed {
        super::super::write_private(&path, text.as_bytes()).map_err(|e| io_error(&path, e))?;
    }
    let applied = (|| {
        if changed {
            ctl_ok(run, &["daemon-reload"])?;
        }
        ctl_ok(run, &["enable", &unit])
    })();
    if let Err(e) = applied {
        if changed {
            // Leave exactly what was there before: a half-registered unit is not a registration.
            match &previous {
                Some(old) => {
                    let _ = super::super::write_private(&path, old.as_bytes());
                }
                None => {
                    let _ = fs::remove_file(&path);
                }
            }
            let _ = ctl(run, &["daemon-reload"]);
        }
        return Err(e);
    }
    Ok(Registration {
        kind: RegistrationKind::SystemdUser,
        name: unit,
        path: Some(path),
    })
}

pub(super) fn start(
    dir: &Path,
    registration: &Registration,
    run: Runner<'_>,
) -> Result<(), AutostartError> {
    let namespace = namespace_of(&registration.name)
        .filter(|_| registration.kind == RegistrationKind::SystemdUser)
        .ok_or_else(|| invalid(format!("{} is not a gateway unit", registration.name)))?;
    if read_owned(&dir.join(&registration.name), namespace)?.is_none() {
        return Err(AutostartError::NotRegistered(namespace.to_owned()));
    }
    ensure_manager(run)?;
    ctl_ok(run, &["start", &registration.name])
}

pub(super) fn inspect(
    dir: &Path,
    startup: &Startup,
    run: Runner<'_>,
) -> Result<Option<Inspection>, AutostartError> {
    let unit = unit_name(&startup.namespace);
    let path = dir.join(&unit);
    let Some(text) = read_owned(&path, &startup.namespace)? else {
        return Ok(None);
    };
    ensure_manager(run)?;
    // `is-enabled` / `is-active` exit non-zero for "disabled" / "inactive": the word is the answer.
    let state = |verb: &str| -> Result<String, AutostartError> {
        Ok(ctl(run, &[verb, &unit])?.stdout.trim().to_owned())
    };
    let enabled = state("is-enabled")? == "enabled";
    let active = Some(matches!(
        state("is-active")?.as_str(),
        "active" | "activating" | "reloading"
    ));
    Ok(Some(Inspection {
        registration: Registration {
            kind: RegistrationKind::SystemdUser,
            name: unit,
            path: Some(path),
        },
        enabled,
        active,
        current: render(startup).is_ok_and(|expected| expected == text),
    }))
}

pub(super) fn remove(dir: &Path, namespace: &str, run: Runner<'_>) -> Result<bool, AutostartError> {
    let unit = unit_name(namespace);
    let path = dir.join(&unit);
    if read_owned(&path, namespace)?.is_none() {
        return Ok(false);
    }
    // Never `--now`: stopping is the supervisor's own decision. Without a manager the wants
    // symlink `enable` made is dropped directly, so no dangling autostart is left behind.
    let disabled = ctl(run, &["disable", &unit]).is_ok_and(|out| out.ok());
    if !disabled {
        let link = dir.join("default.target.wants").join(&unit);
        if fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&link).map_err(|e| io_error(&link, e))?;
        }
    }
    fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
    // Best effort: forgetting the unit does not stop a running instance.
    let _ = ctl(run, &["daemon-reload"]);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn startup() -> Startup {
        Startup {
            namespace: "omp-deck".into(),
            program: PathBuf::from("/opt/Omp Deck/omp-deck-gateway"),
            args: vec![
                "supervise".into(),
                "--startup".into(),
                "/home/u/.config/omp-deck/100%/it's \"q\"$HOME\\x/startup.json".into(),
            ],
        }
    }

    /// Records every systemctl call; `fail` makes the calls that start with it exit 1.
    struct Fake {
        calls: RefCell<Vec<String>>,
        fail: &'static str,
        words: &'static [(&'static str, &'static str)],
    }

    impl Fake {
        fn new(fail: &'static str) -> Fake {
            Fake {
                calls: RefCell::default(),
                fail,
                words: &[],
            }
        }

        fn runner(&self) -> impl Fn(&str, &[&str]) -> Result<Out, AutostartError> + '_ {
            move |program, args| {
                assert_eq!(program, "systemctl");
                let call = args.join(" ");
                self.calls.borrow_mut().push(call.clone());
                let stdout = self
                    .words
                    .iter()
                    .find(|(verb, _)| call.contains(verb))
                    .map_or("", |(_, word)| *word);
                let failed = !self.fail.is_empty() && call.starts_with(self.fail);
                Ok(Out {
                    code: Some(i32::from(failed)),
                    stdout: stdout.into(),
                    stderr: if failed { "boom".into() } else { String::new() },
                })
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    #[test]
    fn unit_survives_spaces_percent_dollar_quotes_and_backslashes() {
        let text = render(&startup()).unwrap();
        assert!(text.contains(
            "ExecStart=\"/opt/Omp Deck/omp-deck-gateway\" \"supervise\" \"--startup\" \
             \"/home/u/.config/omp-deck/100%%/it's \\\"q\\\"$$HOME\\\\x/startup.json\"\n"
        ));
        // The lifetime guarantees the contract depends on.
        for line in [
            "Restart=on-failure",
            "KillMode=process",
            "SendSIGKILL=no",
            "TimeoutStopSec=infinity",
            "WantedBy=default.target",
        ] {
            assert!(text.lines().any(|l| l == line), "{line}");
        }
        assert!(
            text.lines()
                .next()
                .unwrap()
                .starts_with("# Managed by OmpDeck")
        );
    }

    #[test]
    fn unrepresentable_or_unsafe_startups_are_refused() {
        let mut s = startup();
        s.program = PathBuf::from("gateway");
        assert!(matches!(render(&s), Err(AutostartError::Invalid(_))));
        let mut s = startup();
        s.args.push("a\nExecStartPost=/bin/evil".into());
        assert!(matches!(render(&s), Err(AutostartError::Invalid(_))));
        let mut s = startup();
        s.args.push(";".into());
        assert!(matches!(render(&s), Err(AutostartError::Invalid(_))));
        let mut s = startup();
        s.namespace = "bad ns".into();
        assert!(matches!(render(&s), Err(AutostartError::Invalid(_))));
    }

    #[test]
    fn register_is_idempotent_and_never_starts() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let run = fake.runner();
        let reg = register(dir.path(), &startup(), &run).unwrap();
        assert_eq!(reg.kind, RegistrationKind::SystemdUser);
        assert_eq!(reg.name, "omp-deck-gateway-omp-deck.service");
        assert_eq!(
            reg.path.as_deref(),
            Some(dir.path().join(&reg.name).as_path())
        );
        assert_eq!(
            fake.calls(),
            [
                "--user show-environment",
                "--user daemon-reload",
                "--user enable omp-deck-gateway-omp-deck.service"
            ]
        );
        let before = fs::read(reg.path.as_ref().unwrap()).unwrap();
        // Same startup: the file is untouched and nothing is reloaded.
        fake.calls.borrow_mut().clear();
        register(dir.path(), &startup(), &run).unwrap();
        assert_eq!(
            fake.calls(),
            [
                "--user show-environment",
                "--user enable omp-deck-gateway-omp-deck.service"
            ]
        );
        assert_eq!(fs::read(reg.path.as_ref().unwrap()).unwrap(), before);
        // A moved executable repairs the same unit instead of adding a second one.
        let mut moved = startup();
        moved.program = PathBuf::from("/new/omp-deck-gateway");
        register(dir.path(), &moved, &run).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(
            fs::read_to_string(reg.path.unwrap())
                .unwrap()
                .contains("ExecStart=\"/new/omp-deck-gateway\"")
        );
    }

    #[test]
    fn a_foreign_unit_of_the_same_name_is_never_overwritten_or_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("omp-deck-gateway-omp-deck.service");
        fs::write(&path, "[Service]\nExecStart=/bin/mine\n").unwrap();
        let fake = Fake::new("");
        let run = fake.runner();
        assert!(matches!(
            register(dir.path(), &startup(), &run),
            Err(AutostartError::Conflict { .. })
        ));
        assert!(matches!(
            remove(dir.path(), "omp-deck", &run),
            Err(AutostartError::Conflict { .. })
        ));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[Service]\nExecStart=/bin/mine\n"
        );
        assert!(
            fake.calls()
                .iter()
                .all(|c| !c.contains("enable") && !c.contains("disable")),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    fn missing_user_manager_is_unavailable_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("--user show-environment");
        let err = register(dir.path(), &startup(), &fake.runner()).unwrap_err();
        assert!(matches!(err, AutostartError::Unavailable(_)), "{err}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn failed_enable_leaves_no_registration_behind() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("--user enable");
        let err = register(dir.path(), &startup(), &fake.runner()).unwrap_err();
        assert!(matches!(err, AutostartError::Command { .. }), "{err}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        // A failed repair restores the previous unit.
        let ok = Fake::new("");
        register(dir.path(), &startup(), &ok.runner()).unwrap();
        let path = dir.path().join("omp-deck-gateway-omp-deck.service");
        let old = fs::read_to_string(&path).unwrap();
        let mut moved = startup();
        moved.program = PathBuf::from("/new/omp-deck-gateway");
        assert!(register(dir.path(), &moved, &fake.runner()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), old);
    }

    #[test]
    fn remove_disables_and_deletes_without_stopping_anything() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let run = fake.runner();
        assert!(!remove(dir.path(), "omp-deck", &run).unwrap());
        register(dir.path(), &startup(), &run).unwrap();
        fake.calls.borrow_mut().clear();
        assert!(remove(dir.path(), "omp-deck", &run).unwrap());
        let calls = fake.calls();
        assert_eq!(calls[0], "--user disable omp-deck-gateway-omp-deck.service");
        assert!(
            calls
                .iter()
                .all(|c| !c.contains("stop") && !c.contains("--now") && !c.contains("kill")),
            "{calls:?}"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn remove_without_a_manager_still_drops_the_autostart_link() {
        let dir = tempfile::tempdir().unwrap();
        let ok = Fake::new("");
        register(dir.path(), &startup(), &ok.runner()).unwrap();
        let wants = dir.path().join("default.target.wants");
        fs::create_dir(&wants).unwrap();
        let link = wants.join("omp-deck-gateway-omp-deck.service");
        std::os::unix::fs::symlink(dir.path().join("omp-deck-gateway-omp-deck.service"), &link)
            .unwrap();
        let down = Fake::new("--user");
        assert!(remove(dir.path(), "omp-deck", &down.runner()).unwrap());
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(
            !dir.path()
                .join("omp-deck-gateway-omp-deck.service")
                .exists()
        );
    }

    #[test]
    fn inspect_reports_state_and_whether_the_unit_is_current() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            inspect(dir.path(), &startup(), &Fake::new("").runner()).unwrap(),
            None
        );
        register(dir.path(), &startup(), &Fake::new("").runner()).unwrap();
        let mut fake = Fake::new("");
        fake.words = &[("is-enabled", "enabled\n"), ("is-active", "inactive\n")];
        let got = inspect(dir.path(), &startup(), &fake.runner())
            .unwrap()
            .unwrap();
        assert!(got.enabled && got.active == Some(false) && got.current);
        let mut moved = startup();
        moved.program = PathBuf::from("/new/omp-deck-gateway");
        let got = inspect(dir.path(), &moved, &fake.runner())
            .unwrap()
            .unwrap();
        assert!(!got.current);
    }

    #[test]
    fn start_requires_an_owned_unit() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let ghost = Registration {
            kind: RegistrationKind::SystemdUser,
            name: unit_name("omp-deck"),
            path: None,
        };
        assert!(matches!(
            start(dir.path(), &ghost, &fake.runner()),
            Err(AutostartError::NotRegistered(_))
        ));
        let reg = register(dir.path(), &startup(), &fake.runner()).unwrap();
        fake.calls.borrow_mut().clear();
        start(dir.path(), &reg, &fake.runner()).unwrap();
        assert_eq!(
            fake.calls(),
            [
                "--user show-environment",
                "--user start omp-deck-gateway-omp-deck.service"
            ]
        );
    }
}

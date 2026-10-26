//! The Windows scheduled task that supervises the gateway: an interactive-user logon task with
//! least privilege, restart-on-failure and no battery, network, idle or runtime cutoffs.
//!
//! Deleting the task never uses `/End` and the task disallows hard termination, so no process
//! tree (session hosts, omp children) is ever taken down by managing the registration. A startup
//! folder shortcut is not an alternative: it cannot restart a crashed supervisor.

use sha2::{Digest, Sha256};

use super::*;

pub(super) fn task_name(namespace: &str) -> String {
    format!("OmpDeck-Gateway-{namespace}")
}

/// Every task OmpDeck writes carries `omp-deck-gateway:<namespace>:<hash>` as its description;
/// the prefix is the ownership test, the hash tells whether the task is current.
fn description_prefix(namespace: &str) -> String {
    format!("omp-deck-gateway:{namespace}:")
}

/// 16 hex digits over everything the task's behaviour depends on.
fn fingerprint(startup: &Startup, sid: &str) -> String {
    let mut hash = Sha256::new();
    for part in [startup.namespace.as_str(), sid]
        .into_iter()
        .chain(startup.program.to_str())
        .chain(startup.args.iter().map(String::as_str))
    {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `S-1-5-21-…` from the raw SID bytes (revision, sub-authority count, 48-bit big-endian
/// authority, little-endian sub-authorities).
pub(super) fn sid_string(sid: &[u8]) -> Option<String> {
    let (&revision, rest) = sid.split_first()?;
    let (&count, rest) = rest.split_first()?;
    if rest.len() != 6 + 4 * usize::from(count) {
        return None;
    }
    let authority = rest[..6].iter().fold(0u64, |n, b| n << 8 | u64::from(*b));
    let mut out = format!("S-{revision}-{authority}");
    for sub in rest[6..].as_chunks::<4>().0 {
        out.push_str(&format!("-{}", u32::from_le_bytes(*sub)));
    }
    Some(out)
}

/// The current user's SID as the task principal and trigger user.
#[cfg(windows)]
pub(super) fn current_sid() -> Result<String, AutostartError> {
    super::super::imp::user_sid_bytes()
        .and_then(|bytes| sid_string(&bytes))
        .ok_or_else(|| AutostartError::Unavailable("cannot read the current user's SID".into()))
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One argument in the `CommandLineToArgvW` convention.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

pub(super) fn render(startup: &Startup, sid: &str) -> Result<String, AutostartError> {
    let program = startup.validate(super::windows_absolute)?;
    // The scheduler may expand `%VAR%` in the command and arguments; a literal percent sign
    // cannot be expressed, so it is refused instead of being silently rewritten.
    if std::iter::once(program)
        .chain(startup.args.iter().map(String::as_str))
        .any(|t| t.contains('%'))
    {
        return Err(invalid(
            "'%' cannot be used in a scheduled task command".into(),
        ));
    }
    if sid.is_empty() || !sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(invalid(format!("not a SID: {sid:?}")));
    }
    let arguments = if startup.args.is_empty() {
        String::new()
    } else {
        let joined: Vec<String> = startup.args.iter().map(|a| quote_arg(a)).collect();
        format!(
            "      <Arguments>{}</Arguments>\n",
            xml_escape(&joined.join(" "))
        )
    };
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>OmpDeck</Author>
    <Description>{description}{hash}</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
{arguments}    </Exec>
  </Actions>
</Task>
"#,
        description = description_prefix(&startup.namespace),
        hash = fingerprint(startup, sid),
        sid = xml_escape(sid),
        command = xml_escape(program),
    ))
}

/// What `schtasks /Create /XML` reads: UTF-16LE with a byte-order mark.
fn utf16le(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xff, 0xfe];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Console output of `schtasks` as text, whichever encoding it used.
fn decode(bytes: &str) -> String {
    bytes
        .chars()
        .filter(|c| *c != '\0' && *c != '\u{feff}')
        .collect()
}

fn schtasks(run: Runner<'_>, args: &[&str]) -> Result<Out, AutostartError> {
    run("schtasks", args)
}

fn schtasks_ok(run: Runner<'_>, args: &[&str]) -> Result<Out, AutostartError> {
    let out = schtasks(run, args)?;
    if out.ok() {
        Ok(out)
    } else {
        Err(command_error("schtasks", args, &out))
    }
}

/// What is read back of a task in the store.
struct Found {
    description: String,
    enabled: bool,
}

/// The task called `name`; `None` when there is none. The task list is read as CSV, whose first
/// column is not localized, so absence is told apart from failure in every display language.
fn find(name: &str, run: Runner<'_>) -> Result<Option<Found>, AutostartError> {
    let list = schtasks_ok(run, &["/Query", "/FO", "CSV", "/NH"])?;
    let needle = format!("\"\\{name}\"");
    if !decode(&list.stdout)
        .lines()
        .any(|l| l.trim_start().starts_with(&needle))
    {
        return Ok(None);
    }
    let xml = decode(&schtasks_ok(run, &["/Query", "/TN", name, "/XML"])?.stdout);
    // A task without a description still exists; the empty text fails the ownership test.
    let description = xml
        .split_once("<Description>")
        .and_then(|(_, rest)| rest.split_once("</Description>"))
        .map_or("", |(d, _)| d.trim())
        .to_owned();
    let enabled = !xml
        .split_once("<Settings>")
        .is_some_and(|(_, settings)| settings.contains("<Enabled>false</Enabled>"));
    Ok(Some(Found {
        description,
        enabled,
    }))
}

/// The OmpDeck-owned task `name`; `Conflict` when the name belongs to someone else.
fn owned(name: &str, namespace: &str, run: Runner<'_>) -> Result<Option<Found>, AutostartError> {
    match find(name, run)? {
        None => Ok(None),
        Some(found)
            if found
                .description
                .starts_with(&description_prefix(namespace)) =>
        {
            Ok(Some(found))
        }
        Some(_) => Err(AutostartError::Conflict {
            name: name.to_owned(),
        }),
    }
}

fn registration(name: String) -> Registration {
    Registration {
        kind: RegistrationKind::WindowsTask,
        name,
        path: None,
    }
}

pub(super) fn register(
    startup: &Startup,
    sid: &str,
    run: Runner<'_>,
) -> Result<Registration, AutostartError> {
    let xml = render(startup, sid)?;
    let name = task_name(&startup.namespace);
    owned(&name, &startup.namespace, run)?;
    let dir = tempfile::tempdir().map_err(|e| io_error(&std::env::temp_dir(), e))?;
    let file = dir.path().join("gateway-task.xml");
    std::fs::write(&file, utf16le(&xml)).map_err(|e| io_error(&file, e))?;
    let file_arg = file.to_string_lossy();
    schtasks_ok(run, &["/Create", "/TN", &name, "/XML", &file_arg, "/F"])?;
    Ok(registration(name))
}

pub(super) fn start(registration: &Registration, run: Runner<'_>) -> Result<(), AutostartError> {
    let namespace = registration
        .name
        .strip_prefix("OmpDeck-Gateway-")
        .filter(|ns| deck_host::valid_namespace(ns))
        .filter(|_| registration.kind == RegistrationKind::WindowsTask)
        .ok_or_else(|| invalid(format!("{} is not a gateway task", registration.name)))?;
    if owned(&registration.name, namespace, run)?.is_none() {
        return Err(AutostartError::NotRegistered(namespace.to_owned()));
    }
    schtasks_ok(run, &["/Run", "/TN", &registration.name]).map(|_| ())
}

pub(super) fn inspect(
    startup: &Startup,
    sid: &str,
    run: Runner<'_>,
) -> Result<Option<Inspection>, AutostartError> {
    let name = task_name(&startup.namespace);
    let Some(found) = owned(&name, &startup.namespace, run)? else {
        return Ok(None);
    };
    let expected = format!(
        "{}{}",
        description_prefix(&startup.namespace),
        fingerprint(startup, sid)
    );
    Ok(Some(Inspection {
        registration: registration(name),
        enabled: found.enabled,
        active: None,
        current: found.description == expected,
    }))
}

pub(super) fn remove(namespace: &str, run: Runner<'_>) -> Result<bool, AutostartError> {
    let name = task_name(namespace);
    if owned(&name, namespace, run)?.is_none() {
        return Ok(false);
    }
    // No `/End`: deleting a task leaves a running instance (and its descendants) alone.
    schtasks_ok(run, &["/Delete", "/TN", &name, "/F"])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const SID: &str = "S-1-5-21-111-222-333-1001";

    fn startup() -> Startup {
        Startup {
            namespace: "omp-deck".into(),
            program: PathBuf::from(r"C:\Users\A & B\AppData\Local\OmpDeck\omp-deck-gateway.exe"),
            args: vec![
                "supervise".into(),
                "--startup".into(),
                r#"C:\Users\A B\ok "q" dir\"#.into(),
            ],
        }
    }

    #[test]
    fn sid_bytes_become_the_standard_string() {
        let mut bytes = vec![1, 5, 0, 0, 0, 0, 0, 5];
        for sub in [21u32, 111, 222, 333, 1001] {
            bytes.extend_from_slice(&sub.to_le_bytes());
        }
        assert_eq!(sid_string(&bytes).as_deref(), Some(SID));
        assert_eq!(sid_string(&bytes[..bytes.len() - 1]), None);
        assert_eq!(sid_string(&[]), None);
    }

    #[test]
    fn arguments_follow_the_command_line_quoting_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote_arg(r#"x\"y"#), r#""x\\\"y""#);
    }

    #[test]
    fn task_is_a_least_privilege_logon_task_with_no_cutoffs() {
        let xml = render(&startup(), SID).unwrap();
        for needle in [
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<AllowHardTerminate>false</AllowHardTerminate>",
            "<Count>999</Count>",
            "<UserId>S-1-5-21-111-222-333-1001</UserId>",
            // Escaped, with the quoted-argument rules applied before XML escaping.
            r"<Command>C:\Users\A &amp; B\AppData\Local\OmpDeck\omp-deck-gateway.exe</Command>",
            r"<Arguments>supervise --startup &quot;C:\Users\A B\ok \&quot;q\&quot; dir\\&quot;</Arguments>",
        ] {
            assert!(xml.contains(needle), "{needle}\n{xml}");
        }
        assert!(!xml.contains("BootTrigger") && !xml.contains("<RunLevel>Highest"));
    }

    #[test]
    fn a_percent_sign_or_foreign_sid_is_refused() {
        let mut s = startup();
        s.args.push("50%".into());
        assert!(matches!(render(&s, SID), Err(AutostartError::Invalid(_))));
        assert!(matches!(
            render(&startup(), "S-1\"><x"),
            Err(AutostartError::Invalid(_))
        ));
    }

    /// A one-task store behind `schtasks`.
    #[derive(Default)]
    struct Store {
        task: RefCell<Option<String>>,
        calls: RefCell<Vec<String>>,
    }

    impl Store {
        fn runner(&self) -> impl Fn(&str, &[&str]) -> Result<Out, AutostartError> + '_ {
            move |program, args| {
                assert_eq!(program, "schtasks");
                self.calls.borrow_mut().push(args.join(" "));
                let ok = |stdout: String| {
                    Ok(Out {
                        code: Some(0),
                        stdout,
                        stderr: String::new(),
                    })
                };
                match args[0] {
                    "/Query" if args.contains(&"/XML") => ok(self.task.borrow().clone().unwrap()),
                    "/Query" => ok(match &*self.task.borrow() {
                        Some(_) => "\"\\OmpDeck-Gateway-omp-deck\",\"N/A\",\"Ready\"\r\n".into(),
                        None => "\"\\Other\",\"N/A\",\"Ready\"\r\n".into(),
                    }),
                    "/Create" => {
                        let path = args[args.iter().position(|a| *a == "/XML").unwrap() + 1];
                        let bytes = std::fs::read(path).unwrap();
                        assert_eq!(&bytes[..2], [0xff, 0xfe]);
                        let units: Vec<u16> = bytes[2..]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| u16::from_le_bytes(*c))
                            .collect();
                        *self.task.borrow_mut() = Some(String::from_utf16(&units).unwrap());
                        ok(String::new())
                    }
                    "/Delete" => {
                        *self.task.borrow_mut() = None;
                        ok(String::new())
                    }
                    _ => ok(String::new()),
                }
            }
        }
    }

    #[test]
    fn register_repairs_in_place_and_remove_never_ends_the_task() {
        let store = Store::default();
        let run = store.runner();
        let reg = register(&startup(), SID, &run).unwrap();
        assert_eq!(reg.name, "OmpDeck-Gateway-omp-deck");
        assert_eq!(reg.kind, RegistrationKind::WindowsTask);
        let created = store
            .calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("/Create"))
            .count();
        assert_eq!(created, 1);
        assert!(store.calls.borrow().iter().any(|c| c.ends_with("/F")));

        let now = inspect(&startup(), SID, &run).unwrap().unwrap();
        assert!(now.enabled && now.current);
        let mut moved = startup();
        moved.program = PathBuf::from(r"D:\new\omp-deck-gateway.exe");
        assert!(!inspect(&moved, SID, &run).unwrap().unwrap().current);
        register(&moved, SID, &run).unwrap();
        assert!(inspect(&moved, SID, &run).unwrap().unwrap().current);

        start(&reg, &run).unwrap();
        assert!(
            store
                .calls
                .borrow()
                .iter()
                .any(|c| c == "/Run /TN OmpDeck-Gateway-omp-deck")
        );
        assert!(remove("omp-deck", &run).unwrap());
        assert!(!remove("omp-deck", &run).unwrap());
        assert!(
            store.calls.borrow().iter().all(|c| !c.starts_with("/End")),
            "{:?}",
            store.calls
        );
    }

    #[test]
    fn a_foreign_task_of_the_same_name_is_left_alone() {
        let store = Store::default();
        *store.task.borrow_mut() = Some(
            "<Task><RegistrationInfo><Description>mine</Description></RegistrationInfo></Task>"
                .into(),
        );
        let run = store.runner();
        assert!(matches!(
            register(&startup(), SID, &run),
            Err(AutostartError::Conflict { .. })
        ));
        assert!(matches!(
            remove("omp-deck", &run),
            Err(AutostartError::Conflict { .. })
        ));
        assert!(store.task.borrow().as_ref().unwrap().contains("mine"));
        assert!(
            store
                .calls
                .borrow()
                .iter()
                .all(|c| !c.starts_with("/Create") && !c.starts_with("/Delete"))
        );
    }

    #[test]
    fn a_disabled_task_is_reported_disabled() {
        let store = Store::default();
        let run = store.runner();
        register(&startup(), SID, &run).unwrap();
        let off = store.task.borrow().clone().unwrap().replace(
            "<Enabled>true</Enabled>\n    <Hidden>",
            "<Enabled>false</Enabled>\n    <Hidden>",
        );
        *store.task.borrow_mut() = Some(off);
        assert!(!inspect(&startup(), SID, &run).unwrap().unwrap().enabled);
    }
}

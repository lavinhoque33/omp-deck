//! `omp-deck-gateway`: the per-user gateway owner (`serve`), its OS-started supervisor
//! (`supervise`) and the maintenance/inspection commands. Exit codes:
//! [`deck_service::gateway::lifetime::exit`].

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use deck_service::gateway::lifetime::{self, ProfileArgs, exit};
use deck_service::gateway::server::{self, ServeOptions};
use deck_service::gateway::{
    GATEWAY_BIN, GATEWAY_VERSION, MaintenancePurpose, Profile, StartupContext, maintenance_active,
};

const USAGE: &str = "\
usage: omp-deck-gateway <command> [options]

commands:
  serve [--foreground]          run the gateway owner (detaches unless --foreground)
  supervise                     run the OS-started supervisor (foreground)
  status [--json]               show the running owner (exit 3: not running)
  request '<json Request>' [--timeout-secs N]
  watch                         print events as JSON lines
  drain --purpose update|uninstall [--timeout-secs N]
                                raise the maintenance barrier and stop every owner
                                (exit 4: timeout, 6: a direct program still runs)
  resume                        lower the barrier, repair startup contexts, restart supervision
  uninstall [--timeout-secs N]  drain for removal and remove the autostart registration
  --version

profile options (every command):
  --startup <file>              the private startup context (conflicting flags are an error)
  --settings <state.json>
  --sessions-root <dir>
  --ns <host namespace>
";

#[derive(Default)]
struct Args {
    command: Option<String>,
    profile: ProfileArgs,
    foreground: bool,
    json: bool,
    timeout_secs: Option<u64>,
    purpose: Option<MaintenancePurpose>,
    positional: Vec<String>,
    version: bool,
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args::default();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "--version" | "-V" => a.version = true,
            "--startup" => a.profile.startup = Some(PathBuf::from(value("--startup")?)),
            "--settings" => a.profile.settings = Some(PathBuf::from(value("--settings")?)),
            "--sessions-root" => {
                a.profile.sessions_root = Some(PathBuf::from(value("--sessions-root")?));
            }
            "--ns" => a.profile.ns = Some(value("--ns")?),
            "--foreground" => a.foreground = true,
            "--json" => a.json = true,
            "--timeout-secs" => {
                let v = value("--timeout-secs")?;
                a.timeout_secs = Some(
                    v.parse()
                        .map_err(|_| format!("--timeout-secs {v:?} is not a number of seconds"))?,
                );
            }
            "--purpose" => {
                a.purpose = Some(match value("--purpose")?.as_str() {
                    "update" => MaintenancePurpose::Update,
                    "uninstall" => MaintenancePurpose::Uninstall,
                    other => {
                        return Err(format!("--purpose {other:?}: expected update or uninstall"));
                    }
                });
            }
            "-h" | "--help" | "help" => return Err(String::new()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ if a.command.is_none() => a.command = Some(arg),
            _ => a.positional.push(arg),
        }
    }
    Ok(a)
}

/// Rejects options the command does not take.
fn only(
    a: &Args,
    foreground: bool,
    json: bool,
    timeout: bool,
    purpose: bool,
    positional: usize,
) -> Result<(), String> {
    let cmd = a.command.as_deref().unwrap_or("");
    if a.foreground && !foreground {
        return Err(format!("{cmd} does not take --foreground"));
    }
    if a.json && !json {
        return Err(format!("{cmd} does not take --json"));
    }
    if a.timeout_secs.is_some() && !timeout {
        return Err(format!("{cmd} does not take --timeout-secs"));
    }
    if a.purpose.is_some() && !purpose {
        return Err(format!("{cmd} does not take --purpose"));
    }
    if a.positional.len() != positional {
        return Err(format!("{cmd}: unexpected arguments"));
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{GATEWAY_BIN}: {e}");
            }
            eprint!("{USAGE}");
            return code(if e.is_empty() { exit::OK } else { exit::USAGE });
        }
    };
    if args.version {
        println!("{GATEWAY_BIN} {GATEWAY_VERSION}");
        return code(exit::OK);
    }
    code(run(args))
}

fn code(c: i32) -> ExitCode {
    ExitCode::from(u8::try_from(c).unwrap_or(1))
}

fn usage(e: &str) -> i32 {
    eprintln!("{GATEWAY_BIN}: {e}");
    eprint!("{USAGE}");
    exit::USAGE
}

fn run(a: Args) -> i32 {
    let Some(cmd) = a.command.clone() else {
        return usage("missing command");
    };
    let shape = match cmd.as_str() {
        "serve" => only(&a, true, false, false, false, 0),
        "supervise" | "watch" | "resume" => only(&a, false, false, false, false, 0),
        "status" => only(&a, false, true, false, false, 0),
        "request" => only(&a, false, false, true, false, 1),
        "drain" => only(&a, false, false, true, true, 0).and_then(|()| {
            a.purpose
                .map(|_| ())
                .ok_or_else(|| "drain needs --purpose update|uninstall".into())
        }),
        "uninstall" => only(&a, false, false, true, false, 0),
        other => Err(format!("unknown command {other:?}")),
    };
    if let Err(e) = shape {
        return usage(&e);
    }
    let (profile, context) = match lifetime::resolve_profile(&a.profile) {
        Ok(p) => p,
        Err(e) => return usage(&e),
    };
    if cmd == "serve" {
        // Before any thread: the environment and the daemon fork require a single thread.
        return serve(&a, profile, context);
    }
    let timeout = |default: u64| Duration::from_secs(a.timeout_secs.unwrap_or(default));
    match cmd.as_str() {
        "supervise" => supervise(&a, profile, context),
        "status" => {
            lifetime::init_stderr_log();
            lifetime::status(&profile, a.json)
        }
        "request" => {
            lifetime::init_stderr_log();
            lifetime::request(&profile, &a.positional[0], timeout(30))
        }
        "watch" => {
            lifetime::init_stderr_log();
            lifetime::watch(&profile)
        }
        "drain" => {
            lifetime::init_stderr_log();
            let purpose = a.purpose.unwrap_or(MaintenancePurpose::Update);
            lifetime::drain(&profile, purpose, timeout(600))
        }
        "resume" => {
            lifetime::init_stderr_log();
            lifetime::resume(&profile)
        }
        "uninstall" => {
            lifetime::init_stderr_log();
            lifetime::uninstall(&profile, timeout(600))
        }
        _ => unreachable!("validated above"),
    }
}

fn supervise(a: &Args, profile: Profile, context: Option<StartupContext>) -> i32 {
    let file = a
        .profile
        .startup
        .clone()
        .unwrap_or_else(|| StartupContext::path_in(&profile.state_dir()));
    let context = match context {
        Some(c) => c,
        None => match StartupContext::load(&file) {
            Ok(c) if c.profile.same_as(&profile) => c,
            Ok(c) => {
                eprintln!(
                    "{}: the startup context serves another profile ({})",
                    file.display(),
                    c.profile.settings.display()
                );
                return exit::FAILED;
            }
            Err(e) => {
                eprintln!("reading the startup context {}: {e}", file.display());
                return exit::FAILED;
            }
        },
    };
    let log = profile.state_dir().join("supervisor.log");
    if let Err(e) = lifetime::init_file_log(&log) {
        eprintln!("opening {}: {e}", log.display());
        lifetime::init_stderr_log();
    }
    lifetime::supervise(&file, context)
}

fn serve(a: &Args, profile: Profile, context: Option<StartupContext>) -> i32 {
    if let Some(ctx) = &context {
        // SAFETY: nothing has started a thread yet (argument parsing and file reads only).
        unsafe { ctx.apply_env() };
    }
    if let Some(m) = maintenance_active(&profile.app_root()) {
        eprintln!(
            "OmpDeck maintenance ({:?}) is in progress: not starting",
            m.purpose
        );
        return exit::BARRIER;
    }
    let mut waited = 0;
    loop {
        match lifetime::probe_owner(&profile) {
            lifetime::OwnerProbe::Running { pid } => {
                println!("already running pid={pid}");
                return exit::OK;
            }
            lifetime::OwnerProbe::NotRunning | lifetime::OwnerProbe::Unreachable(_) => break,
            lifetime::OwnerProbe::OtherProfile(served) => {
                eprintln!(
                    "the running gateway serves another profile (settings {}, sessions {})",
                    served.settings.display(),
                    served.sessions_root.display()
                );
                return exit::FAILED;
            }
            lifetime::OwnerProbe::Maintenance(m) => {
                eprintln!(
                    "OmpDeck maintenance ({:?}) is in progress: not starting",
                    m.purpose
                );
                return exit::BARRIER;
            }
            // The previous owner is leaving: take over once it is gone.
            lifetime::OwnerProbe::ShuttingDown if waited < 20 => {
                waited += 1;
                std::thread::sleep(Duration::from_millis(500));
            }
            lifetime::OwnerProbe::ShuttingDown => {
                eprintln!("the previous gateway owner did not exit");
                return exit::FAILED;
            }
        }
    }
    #[cfg(unix)]
    let ready_fd = if a.foreground || std::env::var_os("APPIMAGE").is_some_and(|v| !v.is_empty()) {
        // In an AppImage the runtime unmounts the image once its first process exits: stay.
        None
    } else {
        // SAFETY: process entry; no thread has been started (the probe above is a plain
        // blocking socket exchange on this thread).
        match unsafe { deck_host::daemon::daemonize() } {
            Ok(fd) => Some(fd),
            Err(e) => {
                eprintln!("detaching the gateway: {e}");
                return exit::FAILED;
            }
        }
    };
    if a.foreground {
        lifetime::init_stderr_log();
    } else {
        let log = profile.state_dir().join("gateway.log");
        if let Err(e) = lifetime::init_file_log(&log) {
            eprintln!("opening {}: {e}", log.display());
            lifetime::init_stderr_log();
        }
    }
    log::info!(
        "{GATEWAY_BIN} {GATEWAY_VERSION} serving {} (pid {})",
        profile.id().as_str(),
        std::process::id()
    );
    #[cfg(unix)]
    let ready: Option<Box<dyn FnOnce()>> = ready_fd
        .map(|fd| Box::new(move || deck_host::daemon::signal_ready(fd)) as Box<dyn FnOnce()>);
    #[cfg(not(unix))]
    let ready: Option<Box<dyn FnOnce()>> = None;
    server::serve(ServeOptions {
        profile,
        startup_file: a.profile.startup.clone(),
        ready,
    })
}

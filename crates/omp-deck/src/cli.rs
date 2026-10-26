//! Command-line parsing (std only).

use std::path::PathBuf;

pub const HELP: &str = "\
OmpDeck — desktop wrapper for the omp coding-agent TUI

USAGE:
    omp-deck [OPTIONS] [FOLDER]

ARGS:
    [FOLDER]                Per-folder mode: show only the sessions of <FOLDER> (and below)

OPTIONS:
    --folder <dir>          Same as the FOLDER argument
    --sessions-root <dir>   Read omp sessions from <dir> instead of the default store
    --activate <file>       Used by the OmpDeck gateway: open the session of a clicked
                            notification (the file is a private one-use ticket)
    -h, --help              Print this help

ENVIRONMENT:
    OMP_DECK_TMUX_SOCKET    tmux socket name for tests; only `omp-deck-test-…` names are
                            accepted (unset: the production `omp-deck`)
    OMP_DECK_HOST_NS        session-host namespace for tests; only `omp-deck-test-…` names
                            are accepted (unset: the production `omp-deck`)
";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cli {
    pub sessions_root: Option<PathBuf>,
    /// Per-folder mode requested on the command line.
    pub folder: Option<PathBuf>,
    /// Hidden: write a PNG screenshot of the window, then exit.
    pub screenshot: Option<PathBuf>,
    /// Hidden: seconds to wait before taking the screenshot.
    pub screenshot_after: f64,
    /// Set by the gateway when a notification was clicked with no window open: the private
    /// activation ticket to consume (see `deck_service::gateway::activation`).
    pub activate: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum Parsed {
    Run(Cli),
    Help,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut cli = Cli {
        screenshot_after: 5.0,
        ..Cli::default()
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone().or_else(|| it.next()) {
                Some(v) if !v.is_empty() => Ok(v),
                _ => Err(format!("{name} requires a value")),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--sessions-root" => cli.sessions_root = Some(PathBuf::from(value("--sessions-root")?)),
            "--folder" => cli.folder = Some(PathBuf::from(value("--folder")?)),
            "--screenshot" => cli.screenshot = Some(PathBuf::from(value("--screenshot")?)),
            "--activate" => cli.activate = Some(PathBuf::from(value("--activate")?)),
            "--screenshot-after" => {
                let v = value("--screenshot-after")?;
                cli.screenshot_after = v
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .ok_or_else(|| format!("invalid --screenshot-after value: {v}"))?;
            }
            _ if !arg.starts_with('-') => {
                if cli.folder.is_some() {
                    return Err(format!("unexpected extra argument: {arg}"));
                }
                cli.folder = Some(PathBuf::from(arg));
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    Ok(Parsed::Run(cli))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn sessions_root_forms() {
        for args in [&["--sessions-root", "/x"][..], &["--sessions-root=/x"][..]] {
            match p(args) {
                Ok(Parsed::Run(c)) => assert_eq!(c.sessions_root, Some(PathBuf::from("/x"))),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn folder_positional_and_flag() {
        for args in [&["/w"][..], &["--folder", "/w"][..], &["--folder=/w"][..]] {
            match p(args) {
                Ok(Parsed::Run(c)) => assert_eq!(c.folder, Some(PathBuf::from("/w"))),
                other => panic!("{other:?}"),
            }
        }
        assert!(p(&["/a", "/b"]).is_err());
        assert!(p(&["--folder"]).is_err());
    }

    #[test]
    fn help_and_errors() {
        assert_eq!(p(&["--help"]), Ok(Parsed::Help));
        assert!(p(&["--sessions-root"]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert!(p(&["--screenshot-after", "nope"]).is_err());
    }

    #[test]
    fn screenshot_flags() {
        match p(&["--screenshot", "a.png", "--screenshot-after", "2.5"]) {
            Ok(Parsed::Run(c)) => {
                assert_eq!(c.screenshot, Some(PathBuf::from("a.png")));
                assert_eq!(c.screenshot_after, 2.5);
            }
            other => panic!("{other:?}"),
        }
    }
}

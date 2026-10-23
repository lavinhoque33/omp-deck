//! Copy / move a session to another project: target list, destination validation, the native
//! folder picker command and the "Choose folder…" dialog.

use std::path::{Path, PathBuf};

use crate::icons::{ICON_FOLDER_OPEN, ICON_TRIANGLE_ALERT};
use deck_core::sessions::{ProjectGroup, SessionEntry};
use egui::{Id, RichText, TextEdit, Ui, vec2};

use crate::confirm::{Dialog, show_dialog};
use crate::theme::Palette;
use crate::widgets::icon_text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Copy,
    Move,
}

impl TransferKind {
    pub fn verb(self) -> &'static str {
        match self {
            TransferKind::Copy => "Copy",
            TransferKind::Move => "Move",
        }
    }
}

/// Every project except `src_cwd`, newest activity first.
pub fn targets<'a>(groups: &'a [ProjectGroup], src_cwd: &Path) -> Vec<&'a ProjectGroup> {
    let mut out: Vec<&ProjectGroup> = groups.iter().filter(|g| g.cwd != src_cwd).collect();
    out.sort_by_key(|g| std::cmp::Reverse(g.sessions.iter().map(|s| s.modified).max()));
    out
}

/// `~` / `~/x` → under `home`; anything else is taken as typed (trimmed).
pub fn expand_tilde(text: &str, home: Option<&Path>) -> PathBuf {
    let t = text.trim();
    match (t, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (_, Some(h)) if t.starts_with("~/") || (cfg!(windows) && t.starts_with("~\\")) => {
            h.join(t.get(2..).unwrap_or_default())
        }
        _ => PathBuf::from(t),
    }
}

/// Syntax-only part of the destination check (no filesystem access): non-empty, absolute after
/// `~` expansion.
pub fn lexical_dest(text: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if text.trim().is_empty() {
        return Err("Enter the destination folder".to_owned());
    }
    let path = expand_tilde(text, home);
    if !path.is_absolute() {
        return Err("Use an absolute path (or start with ~/)".to_owned());
    }
    Ok(path)
}

/// Blocking filesystem part: an existing directory other than the session's own project.
pub fn check_dest_fs(path: &Path, src_cwd: &Path) -> Result<(), String> {
    if !path.is_dir() {
        return Err("That folder does not exist".to_owned());
    }
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canon(path) == canon(src_cwd) {
        return Err("The session already lives in that folder".to_owned());
    }
    Ok(())
}

/// Native directory picker available on this machine.
#[cfg(not(windows))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picker {
    pub program: &'static str,
    pub args: Vec<String>,
}

/// `kdialog --getexistingdirectory <start>`, else `zenity --file-selection --directory`.
#[cfg(not(windows))]
fn picker_command(start: &Path, have: impl Fn(&str) -> bool) -> Option<Picker> {
    let start = start.to_string_lossy().into_owned();
    if have("kdialog") {
        Some(Picker {
            program: "kdialog",
            args: vec!["--getexistingdirectory".into(), start],
        })
    } else if have("zenity") {
        let mut with_slash = start;
        if !with_slash.ends_with('/') {
            with_slash.push('/');
        }
        Some(Picker {
            program: "zenity",
            args: vec![
                "--file-selection".into(),
                "--directory".into(),
                "--filename".into(),
                with_slash,
            ],
        })
    } else {
        None
    }
}

/// Whether a native folder dialog exists: the system dialog on Windows, else kdialog or zenity.
pub fn picker_available() -> bool {
    cfg!(windows) || has_program("kdialog") || has_program("zenity")
}

fn has_program(name: &str) -> bool {
    deck_core::platform::find_on_path(name).is_some()
}

/// Asks for a folder, starting in `start`. Blocks until the dialog closes: call from a worker.
/// `None` = cancelled (or no dialog available).
pub fn pick_folder(start: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        rfd::FileDialog::new().set_directory(start).pick_folder()
    }
    #[cfg(not(windows))]
    {
        picker_command(start, has_program).and_then(|p| run_picker(&p))
    }
}

/// Runs the picker (blocks until the dialog closes: call from a worker). `None` = cancelled.
#[cfg(not(windows))]
fn run_picker(picker: &Picker) -> Option<PathBuf> {
    let out = std::process::Command::new(picker.program)
        .args(&picker.args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| PathBuf::from(line))
}

/// A filesystem check of the typed destination, to run off the UI thread.
#[derive(Debug, PartialEq, Eq)]
pub struct DestCheck {
    pub generation: u64,
    pub path: PathBuf,
    pub src_cwd: PathBuf,
}

impl DestCheck {
    /// Blocking: `Ok(path)` when it is an existing directory other than the source project.
    pub fn run(self) -> (u64, Result<PathBuf, String>) {
        let result = check_dest_fs(&self.path, &self.src_cwd).map(|()| self.path);
        (self.generation, result)
    }
}

/// Verdict on the typed destination.
enum Checked {
    Invalid(String),
    /// Lexically fine; the worker has not answered yet.
    Pending,
    Valid(PathBuf),
}

/// State of the "Choose folder…" dialog.
pub struct FolderDialog {
    pub kind: TransferKind,
    pub src: SessionEntry,
    text: String,
    checked: Checked,
    /// Bumped with every text change; stale worker answers are dropped.
    generation: u64,
    request: Option<DestCheck>,
    pub browsing: bool,
    focus_first: bool,
}

pub enum FolderOutcome {
    Open,
    Cancel,
    Confirm(PathBuf),
    Browse,
}

impl FolderDialog {
    pub fn new(kind: TransferKind, src: SessionEntry) -> FolderDialog {
        FolderDialog {
            kind,
            src,
            text: String::new(),
            checked: Checked::Invalid("Enter the destination folder".to_owned()),
            generation: 0,
            request: None,
            browsing: false,
            focus_first: true,
        }
    }

    /// Where the native picker should open: the typed folder if valid, else $HOME.
    pub fn start_dir(&self, home: Option<&Path>) -> PathBuf {
        match &self.checked {
            Checked::Valid(p) => Some(p.clone()),
            _ => None,
        }
        .or_else(|| home.map(Path::to_path_buf))
        .unwrap_or_else(deck_core::platform::home_or_root)
    }

    /// New text: the lexical check runs now; the filesystem check is queued for a worker
    /// (see [`FolderDialog::take_request`]).
    pub fn set_text(&mut self, text: String, home: Option<&Path>) {
        self.generation += 1;
        self.request = None;
        self.checked = match lexical_dest(&text, home) {
            Ok(path) => {
                self.request = Some(DestCheck {
                    generation: self.generation,
                    path,
                    src_cwd: self.src.cwd.clone(),
                });
                Checked::Pending
            }
            Err(msg) => Checked::Invalid(msg),
        };
        self.text = text;
    }

    /// The filesystem check to run on a worker, if the text changed since the last call.
    pub fn take_request(&mut self) -> Option<DestCheck> {
        self.request.take()
    }

    /// A worker's verdict; ignored unless it answers the latest text.
    pub fn apply_check(&mut self, generation: u64, result: Result<PathBuf, String>) {
        if generation == self.generation {
            self.checked = match result {
                Ok(p) => Checked::Valid(p),
                Err(msg) => Checked::Invalid(msg),
            };
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        home: Option<&Path>,
        has_picker: bool,
    ) -> FolderOutcome {
        let pal = Palette::of_ctx(ctx);
        let mut outcome = FolderOutcome::Open;
        let mut new_text = None;
        let title = format!("{} to folder", self.kind.verb());
        let confirm_label = self.kind.verb();
        let ok = matches!(self.checked, Checked::Valid(_));
        let answer = show_dialog(
            ctx,
            &Dialog {
                id: "folder",
                title: &title,
                width: 460.0,
                confirm_label,
                confirm_enabled: ok,
                danger: false,
                enter_confirms: true,
            },
            |ui: &mut Ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "Choose the project folder for “{}”.",
                            self.src.label()
                        ))
                        .color(pal.weak),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let browse_w = if has_picker { 108.0 } else { 0.0 };
                    let mut text = self.text.clone();
                    let resp = ui.add(
                        TextEdit::singleline(&mut text)
                            .id(Id::unique("omp-deck-folder-path"))
                            .hint_text("~/projects/app")
                            .font(egui::TextStyle::Monospace)
                            .desired_width(ui.available_width() - browse_w - 8.0)
                            .margin(egui::Margin::symmetric(10, 8)),
                    );
                    if self.focus_first {
                        resp.request_focus();
                        self.focus_first = false;
                    }
                    if resp.changed() {
                        new_text = Some(text);
                    }
                    if has_picker {
                        let label = if self.browsing {
                            RichText::new("Waiting…")
                        } else {
                            RichText::new("Browse…")
                        };
                        let btn = egui::Button::new((
                            icon_text(ICON_FOLDER_OPEN, 17.0, pal.glyph),
                            label,
                        ))
                        .min_size(vec2(browse_w, 34.0));
                        if ui.add_enabled(!self.browsing, btn).clicked() {
                            outcome = FolderOutcome::Browse;
                        }
                    }
                });
                match &self.checked {
                    Checked::Invalid(msg) if !self.text.trim().is_empty() => {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(icon_text(ICON_TRIANGLE_ALERT, 16.0, pal.danger));
                            ui.label(RichText::new(msg).small().color(pal.danger));
                        });
                    }
                    Checked::Pending => {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Checking folder…").small().color(pal.weak));
                    }
                    _ => {}
                }
            },
        );
        if let Some(text) = new_text {
            self.set_text(text, home);
        }
        match answer {
            Some(true) => match &self.checked {
                Checked::Valid(p) => FolderOutcome::Confirm(p.clone()),
                _ => FolderOutcome::Open,
            },
            Some(false) => FolderOutcome::Cancel,
            None => outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    fn group(cwd: &str, age_secs: u64) -> ProjectGroup {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs);
        ProjectGroup {
            cwd: PathBuf::from(cwd),
            display: cwd.into(),
            sessions: vec![SessionEntry {
                id: "i".into(),
                path: PathBuf::from("/s/i.jsonl"),
                cwd: PathBuf::from(cwd),
                title: None,
                first_prompt: None,
                modified,
            }],
        }
    }

    #[test]
    fn targets_exclude_source_and_sort_newest_first() {
        let groups = [group("/a", 500), group("/b", 10), group("/c", 100)];
        let t: Vec<&Path> = targets(&groups, Path::new("/a"))
            .iter()
            .map(|g| g.cwd.as_path())
            .collect();
        assert_eq!(t, [Path::new("/b"), Path::new("/c")]);
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(expand_tilde("~", Some(home)), PathBuf::from("/home/u"));
        assert_eq!(
            expand_tilde(" ~/dev/x ", Some(home)),
            PathBuf::from("/home/u/dev/x")
        );
        assert_eq!(
            expand_tilde("~other/x", Some(home)),
            PathBuf::from("~other/x")
        );
        assert_eq!(expand_tilde("/abs", Some(home)), PathBuf::from("/abs"));
        assert_eq!(expand_tilde("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn destination_validation() {
        let tmp = tempfile::tempdir().expect("tmp");
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir(&src).expect("src");
        std::fs::create_dir(&dest).expect("dest");
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").expect("file");
        let home = Some(tmp.path());

        let validate = |text: &str| -> Result<PathBuf, String> {
            let p = lexical_dest(text, home)?;
            check_dest_fs(&p, &src)?;
            Ok(p)
        };
        assert_eq!(validate("~/dest"), Ok(dest.clone()));
        assert!(validate("").is_err());
        assert!(validate("relative/dir").is_err());
        assert!(validate("~/missing").is_err());
        assert!(validate(&file.to_string_lossy()).is_err());
        assert!(validate(&src.to_string_lossy()).is_err());
        // Same directory through a different spelling.
        let dotted = format!("{}/dest/../src", tmp.path().display());
        assert!(validate(&dotted).is_err());
    }

    #[test]
    fn dialog_ignores_stale_filesystem_answers() {
        let entry = SessionEntry {
            id: "i".into(),
            path: PathBuf::from("/s/i.jsonl"),
            cwd: PathBuf::from("/src"),
            title: None,
            first_prompt: None,
            modified: SystemTime::UNIX_EPOCH,
        };
        let mut dlg = FolderDialog::new(TransferKind::Move, entry);
        let home = Some(Path::new("/home/u"));

        dlg.set_text("/a".into(), home);
        let first = dlg
            .take_request()
            .expect("lexically fine → fs check queued");
        dlg.set_text("/ab".into(), home);
        let second = dlg.take_request().expect("queued");
        assert_ne!(first.generation, second.generation);
        assert!(dlg.take_request().is_none(), "one request per change");

        // The older answer arrives late and must not unlock the dialog.
        dlg.apply_check(first.generation, Ok(PathBuf::from("/a")));
        assert!(matches!(dlg.checked, Checked::Pending));
        dlg.apply_check(second.generation, Ok(PathBuf::from("/ab")));
        assert!(matches!(dlg.checked, Checked::Valid(_)));

        // Lexical failures never reach a worker.
        dlg.set_text("relative".into(), home);
        assert!(dlg.take_request().is_none());
        assert!(matches!(dlg.checked, Checked::Invalid(_)));
    }

    #[cfg(not(windows))]
    #[test]
    fn picker_prefers_kdialog_then_zenity() {
        let start = Path::new("/home/u");
        let both = picker_command(start, |_| true).expect("picker");
        assert_eq!(both.program, "kdialog");
        assert_eq!(both.args, ["--getexistingdirectory", "/home/u"]);
        let zen = picker_command(start, |p| p == "zenity").expect("picker");
        assert_eq!(zen.program, "zenity");
        assert_eq!(zen.args.last().map(String::as_str), Some("/home/u/"));
        assert!(picker_command(start, |_| false).is_none());
    }
}

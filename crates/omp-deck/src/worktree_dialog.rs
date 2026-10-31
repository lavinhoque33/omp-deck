//! The "New session in worktree…" and "Move to new worktree…" dialogs: form state, the checks
//! (the gateway checks the branch name and the destination folder) and the form itself.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::worktree::{self, RepoInfo};
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::DestinationView;
use egui::{Id, RichText, TextEdit, Ui};

use crate::badge::{self, Hue};
use crate::card;
use crate::confirm::{Dialog as Chrome, show_dialog};
use crate::icons::{
    ICON_FOLDER_OPEN, ICON_GIT_BRANCH, ICON_GIT_BRANCH_PLUS, ICON_GIT_FORK, ICON_TRIANGLE_ALERT,
};
use crate::theme::Palette;
use crate::widgets::{DraftButton, icon_text};
use crate::worktree_ui::branch_stamp;

/// Typing is checked this long after the last key stroke (each check runs git).
const DEBOUNCE: Duration = Duration::from_millis(250);

/// What the dialog was opened for.
pub enum Kind {
    /// A new tab in a new worktree of the repository containing `dir`.
    New { dir: PathBuf },
    /// Move the session of tab `uid` (its cwd is `dir`) into a new worktree.
    Move {
        uid: u64,
        /// The session (and generation) the dialog was opened on: the move is typed only into
        /// that conversation.
        target: SessionTarget,
        dir: PathBuf,
        title: String,
    },
}

enum Repo {
    Loading,
    Ready(Box<RepoInfo>),
    Failed(String),
}

/// A verdict that a worker produces.
enum Check<T> {
    Idle,
    Pending,
    Ok(T),
    Bad(String),
}

/// Where the worktree folder comes from.
pub enum Loc {
    /// The gateway's default under omp's managed base (follows the branch name).
    Auto,
    /// What the user typed.
    Typed { text: String, home: Option<PathBuf> },
    /// No folder (moves are placed by omp).
    None,
}

/// A check to run: the gateway judges the branch name and the destination.
pub struct CheckReq {
    pub generation: u64,
    dir: PathBuf,
    branch: String,
    loc: Loc,
}

/// What to ask about the destination folder.
pub enum DestAsk {
    /// Moves have no folder.
    Nothing,
    /// The gateway checks `dest` (`None`: its default location for the branch).
    Gateway { dest: Option<PathBuf>, auto: bool },
    /// The typed text is not a usable path at all.
    Rejected(String),
}

/// The folder part of a check.
pub struct LocationRes {
    location: Option<Result<PathBuf, String>>,
    /// The text of the computed default location.
    auto_text: Option<String>,
}

impl LocationRes {
    /// The gateway's answer for the folder (`auto`: the default location was asked for).
    pub fn answered(auto: bool, res: Result<DestinationView, String>) -> Self {
        match res {
            Ok(v) => Self {
                auto_text: auto.then(|| v.dest.to_string_lossy().into_owned()),
                location: Some(match v.error {
                    None => Ok(v.dest),
                    Some(e) => Err(e),
                }),
            },
            Err(e) => Self {
                location: Some(Err(e)),
                auto_text: None,
            },
        }
    }

    /// A verdict reached without the gateway.
    pub fn rejected(msg: String) -> Self {
        Self {
            location: Some(Err(msg)),
            auto_text: None,
        }
    }

    /// No folder to judge.
    pub fn none() -> Self {
        Self {
            location: None,
            auto_text: None,
        }
    }
}

/// One line for a failure the gateway reported (git's own stderr says it best: the command
/// prefix of its text is dropped).
pub fn brief(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    match line.split_once(" failed: ") {
        Some((_, rest)) if !rest.is_empty() => rest.to_owned(),
        _ => line.to_owned(),
    }
}

impl CheckReq {
    /// The repository whose branch names are checked.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The branch to check (empty = nothing typed yet).
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The branch to ask the gateway about for the folder (its default location follows it).
    pub fn leaf(&self) -> &str {
        if self.branch.is_empty() {
            "worktree"
        } else {
            &self.branch
        }
    }

    /// What to ask the gateway about the destination folder (no I/O).
    pub fn destination(&self) -> DestAsk {
        match &self.loc {
            Loc::None => DestAsk::Nothing,
            Loc::Auto => DestAsk::Gateway {
                dest: None,
                auto: true,
            },
            Loc::Typed { text, home } => match crate::transfer::lexical_dest(text, home.as_deref())
            {
                Ok(p) => DestAsk::Gateway {
                    dest: Some(p),
                    auto: false,
                },
                Err(e) => DestAsk::Rejected(e),
            },
        }
    }
}

/// What the dialog decided on confirm.
pub enum Plan {
    New {
        /// Working tree root the worktree is made from (`omp worktree add -C`).
        repo: PathBuf,
        branch: String,
        base_ref: String,
        /// The folder the user typed or picked; `None`: the gateway's default location.
        dest: Option<PathBuf>,
    },
    Move {
        uid: u64,
        target: SessionTarget,
        branch: String,
    },
}

pub enum Outcome {
    Open,
    Cancel,
    Confirm(Plan),
    Browse,
}

pub struct WorktreeDialog {
    pub kind: Kind,
    repo: Repo,
    branch: String,
    base_ref: String,
    location: String,
    /// The location follows the branch name until the user edits or browses it.
    loc_auto: bool,
    branch_v: Check<()>,
    loc_v: Check<PathBuf>,
    /// Bumped with every edit; stale worker answers are dropped.
    generation: u64,
    needs_check: bool,
    edited_at: Instant,
    pub browsing: bool,
    focus_first: bool,
}

impl WorktreeDialog {
    pub fn new(kind: Kind, now_unix: u64) -> WorktreeDialog {
        WorktreeDialog {
            kind,
            repo: Repo::Loading,
            branch: branch_stamp(now_unix),
            base_ref: String::new(),
            location: String::new(),
            loc_auto: true,
            branch_v: Check::Idle,
            loc_v: Check::Idle,
            generation: 0,
            needs_check: false,
            edited_at: Instant::now(),
            browsing: false,
            focus_first: true,
        }
    }

    /// The directory whose repository the worktree is made from.
    pub fn dir(&self) -> &Path {
        match &self.kind {
            Kind::New { dir } | Kind::Move { dir, .. } => dir,
        }
    }

    fn is_move(&self) -> bool {
        matches!(self.kind, Kind::Move { .. })
    }

    /// The repository facts arrived.
    pub fn on_repo(&mut self, result: Result<RepoInfo, String>) {
        match result {
            Ok(info) => {
                self.base_ref = info
                    .current_branch
                    .clone()
                    .unwrap_or_else(|| "HEAD".to_owned());
                self.repo = Repo::Ready(Box::new(info));
                self.edited();
            }
            Err(e) => self.repo = Repo::Failed(brief(&e)),
        }
    }

    fn edited(&mut self) {
        self.generation += 1;
        self.needs_check = true;
        self.edited_at = Instant::now();
        self.branch_v = Check::Pending;
        if !self.is_move() {
            self.loc_v = Check::Pending;
        }
    }

    /// The check to run now, if one is due (the repository is known and typing paused).
    pub fn take_check(&mut self, home: Option<&Path>) -> Option<CheckReq> {
        let Repo::Ready(_) = &self.repo else {
            return None;
        };
        if !self.needs_check || self.edited_at.elapsed() < DEBOUNCE {
            return None;
        }
        self.needs_check = false;
        let loc = match (&self.kind, self.loc_auto) {
            (Kind::Move { .. }, _) => Loc::None,
            (_, true) => Loc::Auto,
            (_, false) => Loc::Typed {
                text: self.location.clone(),
                home: home.map(Path::to_path_buf),
            },
        };
        Some(CheckReq {
            generation: self.generation,
            dir: self.dir().to_path_buf(),
            branch: self.branch.trim().to_owned(),
            loc,
        })
    }

    /// When the next check is due (to repaint then).
    pub fn check_due_in(&self) -> Option<Duration> {
        (self.needs_check && matches!(self.repo, Repo::Ready(_)))
            .then(|| DEBOUNCE.saturating_sub(self.edited_at.elapsed()))
    }

    /// The gateway's verdict on the branch name; ignored unless it answers the latest edit.
    pub fn apply_branch(&mut self, generation: u64, res: Result<(), String>) {
        if generation != self.generation {
            return;
        }
        self.branch_v = match res {
            Ok(()) => Check::Ok(()),
            Err(e) => Check::Bad(e),
        };
    }

    /// The destination folder's verdict; ignored unless it answers the latest edit.
    pub fn apply_location(&mut self, generation: u64, res: LocationRes) {
        if generation != self.generation {
            return;
        }
        if let Some(text) = res.auto_text
            && self.loc_auto
        {
            self.location = text;
        }
        if let Some(loc) = res.location {
            self.loc_v = match loc {
                Ok(p) => Check::Ok(p),
                Err(e) => Check::Bad(e),
            };
        }
    }

    /// Where the native folder dialog opens.
    pub fn start_dir(&self, home: Option<&Path>) -> PathBuf {
        match &self.loc_v {
            Check::Ok(p) => p.parent().map(Path::to_path_buf),
            _ => None,
        }
        .or_else(|| home.map(Path::to_path_buf))
        .unwrap_or_else(deck_core::platform::home_or_root)
    }

    /// The user picked a folder to create the worktree in.
    pub fn picked(&mut self, parent: &Path) {
        let primary = match &self.repo {
            Repo::Ready(i) => i.primary.clone(),
            _ => self.dir().to_path_buf(),
        };
        let leaf = format!(
            "{}-{}",
            worktree::slug(self.branch.trim()),
            worktree::slug(
                &primary
                    .file_name()
                    .map_or_else(|| "repo".to_owned(), |n| n.to_string_lossy().into_owned())
            )
        );
        self.location = parent.join(leaf).to_string_lossy().into_owned();
        self.loc_auto = false;
        self.edited();
    }

    fn confirmable(&self) -> bool {
        let Repo::Ready(info) = &self.repo else {
            return false;
        };
        info.has_commits
            && matches!(self.branch_v, Check::Ok(()))
            && (self.is_move() || matches!(self.loc_v, Check::Ok(_)))
    }

    fn plan(&self) -> Option<Plan> {
        if !self.confirmable() {
            return None;
        }
        let Repo::Ready(info) = &self.repo else {
            return None;
        };
        let branch = self.branch.trim().to_owned();
        match (&self.kind, &self.loc_v) {
            (Kind::Move { uid, target, .. }, _) => Some(Plan::Move {
                uid: *uid,
                target: target.clone(),
                branch,
            }),
            (Kind::New { .. }, Check::Ok(dest)) => Some(Plan::New {
                repo: info.root.clone(),
                base_ref: self.base_ref.clone(),
                branch,
                dest: (!self.loc_auto).then(|| dest.clone()),
            }),
            _ => None,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, has_picker: bool) -> Outcome {
        let pal = Palette::of_ctx(ctx);
        let mut outcome = Outcome::Open;
        let (title, label, icon, subtitle) = match &self.kind {
            Kind::New { dir } => (
                "New session in worktree",
                "Create",
                ICON_GIT_BRANCH_PLUS,
                format!(
                    "Clean checkout of {}",
                    crate::folder_scope::folder_name(dir)
                ),
            ),
            Kind::Move { title, .. } => (
                "Move to a new worktree",
                "Move",
                ICON_GIT_FORK,
                format!("Moves “{title}”"),
            ),
        };
        let enabled = self.confirmable();
        let mut branch_edited = false;
        let mut loc_edited = false;
        let answer = show_dialog(
            ctx,
            &Chrome {
                id: "worktree",
                title,
                width: 480.0,
                confirm_label: label,
                confirm_enabled: enabled,
                danger: false,
                enter_confirms: true,
                icon: Some(icon),
                subtitle: Some(&subtitle),
                ..Default::default()
            },
            |ui: &mut Ui| {
                self.intro(ui, pal);
                match &self.repo {
                    Repo::Loading => {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add(crate::widgets::Spin::new(16.0, pal.accent));
                            ui.label(RichText::new("Reading the repository…").color(pal.weak));
                        });
                        return;
                    }
                    Repo::Failed(msg) => {
                        ui.add_space(10.0);
                        problem(ui, pal, msg);
                        return;
                    }
                    Repo::Ready(info) if !info.has_commits => {
                        ui.add_space(10.0);
                        problem(
                            ui,
                            pal,
                            "This repository has no commits yet — make one first, git cannot branch from nothing.",
                        );
                        return;
                    }
                    Repo::Ready(_) => {}
                }
                ui.add_space(12.0);
                card::section(ui, "Branch");
                let resp = ui.add(
                    TextEdit::singleline(&mut self.branch)
                        .id(Id::unique("omp-deck-worktree-branch"))
                        .hint_text("wt/my-feature")
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(10, 8)),
                );
                if self.focus_first {
                    resp.request_focus();
                    self.focus_first = false;
                }
                branch_edited = resp.changed();
                verdict(ui, pal, &self.branch_v, "Checking the name…");
                if self.is_move() {
                    self.move_details(ui, pal);
                    return;
                }
                ui.add_space(10.0);
                card::section(ui, "Based on");
                self.base_picker(ui);
                ui.add_space(10.0);
                card::section(ui, "Folder");
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let browse_w = if has_picker { 108.0 } else { 0.0 };
                    let resp = ui.add(
                        TextEdit::singleline(&mut self.location)
                            .id(Id::unique("omp-deck-worktree-folder"))
                            .font(egui::TextStyle::Monospace)
                            .desired_width(ui.available_width() - browse_w - 8.0)
                            .margin(egui::Margin::symmetric(10, 8)),
                    );
                    loc_edited = resp.changed();
                    if has_picker {
                        let text = if self.browsing {
                            "Waiting…"
                        } else {
                            "Browse…"
                        };
                        if DraftButton::new(text)
                            .icon(ICON_FOLDER_OPEN)
                            .enabled(!self.browsing)
                            .min_width(browse_w)
                            .show(ui)
                            .on_hover_text("Choose the folder to create the worktree in")
                            .clicked()
                        {
                            outcome = Outcome::Browse;
                        }
                    }
                });
                verdict(ui, pal, &self.loc_v, "Checking the folder…");
            },
        );
        if branch_edited {
            self.edited();
        }
        if loc_edited {
            self.loc_auto = false;
            self.edited();
        }
        match answer {
            Some(true) => self.plan().map_or(Outcome::Open, Outcome::Confirm),
            Some(false) => Outcome::Cancel,
            None => outcome,
        }
    }

    fn intro(&self, ui: &mut Ui, pal: &Palette) {
        let text = match &self.kind {
            Kind::New { dir } => format!(
                "Creates a git worktree of “{}” on a new branch and starts an omp session in it. The worktree is a clean checkout.",
                crate::folder_scope::folder_name(dir)
            ),
            Kind::Move { title, .. } => format!(
                "Moves “{title}” into a new worktree: omp creates it, carries your uncommitted changes along and the session continues there."
            ),
        };
        ui.add(
            egui::Label::new(RichText::new(text).color(pal.weak))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }

    fn move_details(&self, ui: &mut Ui, pal: &Palette) {
        let Repo::Ready(info) = &self.repo else {
            return;
        };
        ui.add_space(8.0);
        card::kv(ui, 92.0, "Merges into", |ui| match &info.current_branch {
            Some(b) => {
                badge::show(ui, Some(ICON_GIT_BRANCH), b, Hue::Teal, 11.0);
            }
            None => {
                badge::show(ui, None, "detached HEAD", Hue::Amber, 11.0);
            }
        });
        ui.add_space(4.0);
        let text = match &info.current_branch {
            Some(b) => format!("Based on the current branch “{b}”, which is also where it is merged back."),
            None => "The repository is on a detached HEAD: OmpDeck will look for the merge target when you finish the worktree.".to_owned(),
        };
        ui.add(
            egui::Label::new(RichText::new(text).small().color(pal.weak))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }

    fn base_picker(&mut self, ui: &mut Ui) {
        let Repo::Ready(info) = &self.repo else {
            return;
        };
        egui::ComboBox::from_id_salt("omp-deck-worktree-base")
            .selected_text(self.base_ref.as_str())
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        if info.current_branch.is_none() {
                            ui.selectable_value(&mut self.base_ref, "HEAD".to_owned(), "HEAD");
                        }
                        for b in &info.branches {
                            ui.selectable_value(&mut self.base_ref, b.clone(), b);
                        }
                    });
            });
    }
}

fn problem(ui: &mut Ui, pal: &Palette, msg: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(icon_text(ICON_TRIANGLE_ALERT, 16.0, pal.danger));
        ui.label(RichText::new(msg).small().color(pal.danger));
    });
}

fn verdict<T>(ui: &mut Ui, pal: &Palette, check: &Check<T>, pending: &str) {
    match check {
        Check::Bad(msg) => {
            ui.add_space(4.0);
            problem(ui, pal, msg);
        }
        Check::Pending => {
            ui.add_space(4.0);
            ui.label(RichText::new(pending).small().color(pal.weak));
        }
        Check::Idle | Check::Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_service::controller::SessionKey;

    fn info(has_commits: bool) -> RepoInfo {
        RepoInfo {
            root: PathBuf::from("/p/repo"),
            primary: PathBuf::from("/p/repo"),
            current_branch: Some("main".into()),
            branches: vec!["main".into(), "dev".into(), "origin/main".into()],
            has_commits,
        }
    }

    fn new_dialog() -> WorktreeDialog {
        WorktreeDialog::new(
            Kind::New {
                dir: PathBuf::from("/p/repo"),
            },
            0,
        )
    }

    #[test]
    fn the_branch_is_prefilled_and_checks_wait_for_the_repository_and_the_typing_pause() {
        let mut d = new_dialog();
        assert_eq!(d.branch, "wt/19700101-0000");
        assert!(d.take_check(None).is_none(), "repository not read yet");
        d.on_repo(Ok(info(true)));
        assert_eq!(d.base_ref, "main");
        assert!(d.take_check(None).is_none(), "typing pause not over");
        d.edited_at -= DEBOUNCE;
        let req = d.take_check(None).expect("due");
        assert_eq!(req.branch, "wt/19700101-0000");
        assert!(matches!(req.loc, Loc::Auto));
        assert!(d.take_check(None).is_none(), "one check per edit");
    }

    #[test]
    fn stale_answers_cannot_unlock_the_dialog() {
        let mut d = new_dialog();
        d.on_repo(Ok(info(true)));
        d.edited_at -= DEBOUNCE;
        let first = d.take_check(None).expect("check");
        d.edited(); // the user kept typing
        let answer = |d: &mut WorktreeDialog, generation, location: &str| {
            d.apply_branch(generation, Ok(()));
            d.apply_location(
                generation,
                LocationRes {
                    location: Some(Ok(PathBuf::from(location))),
                    auto_text: Some(location.to_owned()),
                },
            );
        };
        answer(&mut d, first.generation, "/wt/old");
        assert!(!d.confirmable());
        assert!(d.location.is_empty());
        let current = d.generation;
        answer(&mut d, current, "/wt/new");
        assert!(d.confirmable());
        assert_eq!(d.location, "/wt/new");
    }

    #[test]
    fn a_typed_folder_stops_following_the_branch_and_is_what_gets_created() {
        let mut d = new_dialog();
        d.on_repo(Ok(info(true)));
        d.location = "/typed/here".into();
        d.loc_auto = false;
        d.edited();
        d.edited_at -= DEBOUNCE;
        let req = d.take_check(Some(Path::new("/home/u"))).expect("check");
        assert!(matches!(&req.loc, Loc::Typed { text, .. } if text == "/typed/here"));
        d.apply_branch(req.generation, Ok(()));
        d.apply_location(
            req.generation,
            LocationRes {
                location: Some(Ok(PathBuf::from("/typed/here"))),
                auto_text: Some("/auto/ignored".into()),
            },
        );
        assert_eq!(
            d.location, "/typed/here",
            "an auto text never overwrites the user's"
        );
        let Some(Plan::New { dest, base_ref, .. }) = d.plan() else {
            panic!("plan");
        };
        assert_eq!(dest.as_deref(), Some(Path::new("/typed/here")));
        assert_eq!(base_ref, "main");
        // Until the user edits it the gateway picks the folder.
        d.loc_auto = true;
        let Some(Plan::New { dest, .. }) = d.plan() else {
            panic!("plan");
        };
        assert_eq!(dest, None);
    }

    #[test]
    fn a_bad_name_or_a_repository_without_commits_blocks_confirmation() {
        let mut d = new_dialog();
        d.on_repo(Ok(info(true)));
        d.apply_branch(d.generation, Err("branch 'x' already exists".into()));
        d.apply_location(
            d.generation,
            LocationRes {
                location: Some(Ok(PathBuf::from("/wt/x"))),
                auto_text: None,
            },
        );
        assert!(d.plan().is_none());

        let mut empty = new_dialog();
        empty.on_repo(Ok(info(false)));
        empty.apply_branch(empty.generation, Ok(()));
        empty.apply_location(
            empty.generation,
            LocationRes {
                location: Some(Ok(PathBuf::from("/wt/x"))),
                auto_text: None,
            },
        );
        assert!(empty.plan().is_none());
    }

    #[test]
    fn a_move_needs_no_folder_and_merges_back_into_the_current_branch() {
        let mut d = WorktreeDialog::new(
            Kind::Move {
                uid: 7,
                target: SessionTarget {
                    epoch: deck_service::controller::ServiceEpoch([0; 16]),
                    key: SessionKey::tmux("test", "s"),
                    generation: 1,
                },
                dir: PathBuf::from("/p/repo"),
                title: "t".into(),
            },
            0,
        );
        d.on_repo(Ok(info(true)));
        d.edited_at -= DEBOUNCE;
        let req = d.take_check(None).expect("check");
        assert!(matches!(req.loc, Loc::None));
        d.apply_branch(req.generation, Ok(()));
        d.apply_location(
            req.generation,
            LocationRes {
                location: None,
                auto_text: None,
            },
        );
        let Some(Plan::Move { uid, branch, .. }) = d.plan() else {
            panic!("plan");
        };
        assert_eq!((uid, branch.as_str()), (7, "wt/19700101-0000"));
    }
}

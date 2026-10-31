//! Settings → Agents: the task agents omp offers (see [`deck_core::agent_defs`]): its built-ins,
//! the user's (`<agent dir>/agents`) and each known project's `.omp/agents`.
//!
//! - A definition file: its model (the status bar's model dropdown, written to the file's `model`
//!   entry) and system prompt (the file's body) are edited in place; it can be deleted.
//! - A built-in is read-only: its model is overridden in omp's config
//!   (`task.agentModelOverrides`), and "Customize" copies it into the user's or a project's
//!   agents, where the copy replaces it and is edited like any file.
//! - Any agent can be disabled (`task.disabledAgents`); "New agent" writes a new file.
//!
//! Pure UI: state in, [`AgentsAction`]s out; `app/agent_config.rs` runs them over the gateway.
//! Unsaved prompt edits are kept per file while the app runs; a file that changed on disk under
//! them is never overwritten silently (the editor asks which version to keep).

mod create;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use deck_core::agent_defs::{
    self, AgentDir, AgentFile, AgentSettings, Catalog, DISABLED_KEY, Edit, NewAgent, OVERRIDES_KEY,
    Scope,
};
use deck_core::sessions::abbreviate_home;
use deck_service::statusfmt::{short_model_name, thinking_short};
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Key, KeyboardShortcut, Layout, Margin, Modifiers,
    Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2,
};

use self::create::{CreateDialog, Outcome, Target};
use crate::badge::{self, Hue};
use crate::confirm::{self, Confirm};
use crate::icons::{
    ICON_BOT, ICON_CHEVRON_DOWN, ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_ALERT, ICON_CIRCLE_PAUSE,
    ICON_COPY, ICON_EXTERNAL_LINK, ICON_FILE_PEN_LINE, ICON_FILE_PLUS, ICON_FOLDER,
    ICON_FOLDER_OPEN, ICON_INFO, ICON_PACKAGE, ICON_PLUS, ICON_REFRESH_CW, ICON_SAVE, ICON_SEARCH,
    ICON_TRASH_2, ICON_TRIANGLE_ALERT, ICON_UNDO_2, ICON_USER, ICON_X, Icon,
};
use crate::model_picker::{self, ModelsCache, PickerAction, PickerInput, PickerState};
use crate::omp_settings::split_level;
use crate::theme::{self, Palette};
use crate::widgets::{
    DraftButton, DraftKind, Spin, card_frame, empty_state, icon_button, icon_text, menu_item,
    paint_icon, toggle,
};

#[derive(Debug, PartialEq)]
pub enum AgentsAction {
    /// (Re)read the definitions, the built-ins and omp's agent settings.
    Load,
    Edit {
        path: PathBuf,
        revision: String,
        edit: Edit,
    },
    /// Write one of omp's agent settings (`task.agentModelOverrides`, `task.disabledAgents`)
    /// as `value`; `done` is the notice once written.
    SetSetting {
        key: &'static str,
        value: serde_json::Value,
        done: String,
    },
    /// Write a new definition into `dir`.
    Create {
        dir: PathBuf,
        agent: NewAgent,
    },
    /// Copy omp's built-in `name` into `dir`, where the copy replaces it.
    Customize {
        dir: PathBuf,
        name: String,
    },
    Delete {
        path: PathBuf,
        revision: String,
        name: String,
    },
    /// The model dropdown's own controls: reload the catalog, star a model, filter by provider.
    ReloadModels,
    ToggleFavorite(String),
    ModelProvider(Option<String>),
    OpenFile(PathBuf),
    OpenFolder(PathBuf),
    CopyPath(String),
}

/// Which part of a file an edit in flight changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Model,
    Body,
}

impl Part {
    pub fn of(edit: &Edit) -> Part {
        match edit {
            Edit::Model(_) => Part::Model,
            Edit::Body(_) => Part::Body,
        }
    }
}

pub struct AgentsInput<'a> {
    pub models: &'a ModelsCache,
    pub favorites: &'a [String],
    pub provider: Option<&'a str>,
}

/// A loaded catalog and what the list shows of it (computed once per load or edit).
struct Listing {
    catalog: Catalog,
    entries: Vec<ListEntry>,
    /// Where new and customized agents can go.
    targets: Vec<Target>,
}

struct ListEntry {
    dir: usize,
    idx: usize,
    name: String,
    /// Lowercase name, description, model and file name (the filter's haystack).
    haystack: String,
    /// How it relates to agents of the same name elsewhere.
    relation: Option<String>,
    /// A built-in: the file that replaces it (the user's, else a project's) and whether it
    /// does so in every session (a user file).
    replaced_by: Option<(PathBuf, bool)>,
}

impl Listing {
    fn new(catalog: Catalog) -> Listing {
        let roots: Vec<String> = catalog.dirs.iter().map(scope_title).collect();
        // Who declares which name: (name, dir index, file), in catalog order.
        let declared: Vec<(&str, usize, &Path)> = catalog
            .dirs
            .iter()
            .enumerate()
            .flat_map(|(di, d)| {
                d.agents
                    .iter()
                    .filter_map(move |a| a.loaded_name().map(|n| (n, di, a.path.as_path())))
            })
            .collect();
        let in_scope = |name: &str, scope: Scope| -> Vec<(usize, &Path)> {
            declared
                .iter()
                .filter(|(n, di, _)| *n == name && catalog.dirs[*di].scope == scope)
                .map(|(_, di, p)| (*di, *p))
                .collect()
        };
        let project_titles = |name: &str| {
            in_scope(name, Scope::Project)
                .iter()
                .map(|(di, _)| roots[*di].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut entries = Vec::new();
        for (di, dir) in catalog.dirs.iter().enumerate() {
            for (ai, a) in dir.agents.iter().enumerate() {
                let mut notes = Vec::new();
                let mut replaced_by = None;
                if let Some(name) = a.loaded_name() {
                    let builtin = !in_scope(name, Scope::Bundled).is_empty();
                    match dir.scope {
                        Scope::Project => {
                            if !in_scope(name, Scope::User).is_empty() {
                                notes.push(
                                    "Replaces your user agent of the same name in this project"
                                        .to_owned(),
                                );
                            }
                            if builtin {
                                notes.push(format!(
                                    "Replaces omp's built-in {name} in this project"
                                ));
                            }
                        }
                        Scope::User => {
                            let projects = project_titles(name);
                            if !projects.is_empty() {
                                notes.push(format!(
                                    "A project agent of the same name replaces it in {projects}"
                                ));
                            }
                            if builtin {
                                notes.push(format!("Replaces omp's built-in {name}"));
                            }
                        }
                        Scope::Bundled => {
                            if let Some(&(_, user)) = in_scope(name, Scope::User).first() {
                                notes.push(format!(
                                    "Your user agent {} replaces it",
                                    user.file_name().unwrap_or_default().to_string_lossy()
                                ));
                                replaced_by = Some((user.to_path_buf(), true));
                            } else if let Some(&(_, p)) = in_scope(name, Scope::Project).first() {
                                notes.push(format!(
                                    "A project agent replaces it in {}",
                                    project_titles(name)
                                ));
                                replaced_by = Some((p.to_path_buf(), false));
                            }
                        }
                    }
                }
                let file = a.path.file_name().unwrap_or_default().to_string_lossy();
                let haystack = format!(
                    "{} {} {} {file}{}",
                    a.display_name(),
                    a.description.as_deref().unwrap_or(""),
                    a.model.as_deref().unwrap_or(""),
                    if dir.scope == Scope::Bundled {
                        " built-in"
                    } else {
                        ""
                    },
                )
                .to_lowercase();
                entries.push(ListEntry {
                    dir: di,
                    idx: ai,
                    name: a.display_name(),
                    haystack,
                    relation: (!notes.is_empty()).then(|| notes.join("\n")),
                    replaced_by,
                });
            }
        }
        let targets = create::targets(&catalog);
        Listing {
            catalog,
            entries,
            targets,
        }
    }

    fn file(&self, path: &Path) -> Option<&AgentFile> {
        self.catalog
            .dirs
            .iter()
            .flat_map(|d| &d.agents)
            .find(|a| a.path == path)
    }

    fn dir_of(&self, path: &Path) -> Option<&AgentDir> {
        self.catalog
            .dirs
            .iter()
            .find(|d| d.agents.iter().any(|a| a.path == path))
    }

    fn entry(&self, path: &Path) -> Option<&ListEntry> {
        self.entries
            .iter()
            .find(|e| self.catalog.dirs[e.dir].agents[e.idx].path == path)
    }

    fn settings(&self) -> Option<&AgentSettings> {
        self.catalog.settings.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// The first definition of `scope` that omp loads as `name`.
    fn declaring(&self, scope: Scope, name: &str) -> Option<&AgentFile> {
        self.catalog
            .dirs
            .iter()
            .filter(|d| d.scope == scope)
            .flat_map(|d| &d.agents)
            .find(|a| a.loaded_name() == Some(name))
    }

    /// Swaps in a re-read file (same path), keeping the rest.
    fn replace(self, file: AgentFile) -> Listing {
        let mut catalog = self.catalog;
        if let Some(slot) = catalog
            .dirs
            .iter_mut()
            .flat_map(|d| d.agents.iter_mut())
            .find(|a| a.path == file.path)
        {
            *slot = file;
        }
        Listing::new(catalog)
    }
}

/// `User` / the project folder's name / `Built-in`.
fn scope_title(dir: &AgentDir) -> String {
    match dir.scope {
        Scope::User => "User".to_owned(),
        Scope::Project => dir.root.file_name().map_or_else(
            || abbreviate_home(&dir.root),
            |n| n.to_string_lossy().into_owned(),
        ),
        Scope::Bundled => "Built-in".to_owned(),
    }
}

/// Unsaved prompt edits of one file.
struct Draft {
    text: String,
    /// The body and revision the text was started from (or last saved as).
    base_body: String,
    base_revision: String,
    /// The file changed on disk under unsaved edits: its revision now.
    disk_revision: Option<String>,
}

impl Draft {
    fn of(file: &AgentFile) -> Draft {
        Draft {
            text: file.body.clone(),
            base_body: file.body.clone(),
            base_revision: file.revision.clone(),
            disk_revision: None,
        }
    }

    fn dirty(&self) -> bool {
        self.text.trim() != self.base_body
    }
}

/// A delete waiting for the user's confirmation.
#[derive(Debug)]
struct DeleteAsk {
    path: PathBuf,
    revision: String,
    name: String,
    /// What applies once the file is gone.
    after: String,
    /// The definition to select afterwards (the one that applies again).
    then: Option<PathBuf>,
}

#[derive(Default)]
pub struct AgentsState {
    listing: Option<Listing>,
    loading: bool,
    load_error: Option<String>,
    /// Load on the next draw (the page was opened, or an edit found the file changed).
    want_load: bool,
    selected: Option<PathBuf>,
    /// Select this definition once a load lists it (one just created, or the built-in a
    /// deleted copy gives way to).
    select_after_load: Option<PathBuf>,
    filter: String,
    /// The selection the list last showed: a different one (a new file, the definition a
    /// delete gives way to, "Show it") is scrolled into view.
    revealed: Option<PathBuf>,
    drafts: HashMap<PathBuf, Draft>,
    picker: PickerState,
    /// The file with an edit in flight.
    busy: Option<(PathBuf, Part)>,
    /// The last failed edit: file and message.
    failure: Option<(PathBuf, String)>,
    /// A write to omp's agent settings is in flight.
    setting_busy: bool,
    /// The built-in being copied by "Customize".
    customizing: Option<PathBuf>,
    /// The "New agent" dialog, when open.
    create: Option<CreateDialog>,
    ask_delete: Option<DeleteAsk>,
    /// The file being deleted and the built-in to select afterwards.
    deleting: Option<(PathBuf, Option<PathBuf>)>,
}

impl AgentsState {
    /// The page is being shown (again): reread the files so edits made elsewhere show up.
    pub fn opened(&mut self) {
        self.want_load = true;
    }

    pub fn loaded(&mut self, result: Result<Catalog, String>) {
        self.loading = false;
        let catalog = match result {
            Ok(c) => c,
            Err(e) => {
                self.load_error = Some(e);
                return;
            }
        };
        self.load_error = None;
        let listing = Listing::new(catalog);
        // Clean drafts follow the disk; dirty ones learn that it moved on under them.
        self.drafts.retain(|path, d| match listing.file(path) {
            None => false,
            Some(f) if f.revision == d.base_revision => {
                d.disk_revision = None;
                true
            }
            Some(f) if d.dirty() => {
                d.disk_revision = Some(f.revision.clone());
                true
            }
            Some(_) => false,
        });
        if let Some(p) = self.select_after_load.take()
            && listing.file(&p).is_some()
        {
            self.selected = Some(p);
        }
        if self
            .selected
            .as_deref()
            .is_none_or(|p| listing.file(p).is_none())
        {
            self.selected = listing
                .entries
                .first()
                .map(|e| listing.catalog.dirs[e.dir].agents[e.idx].path.clone());
        }
        self.listing = Some(listing);
    }

    /// An edit of `path` ended; `Ok` carries the file as now on disk.
    pub fn edit_done(&mut self, path: &Path, part: Part, result: Result<AgentFile, String>) {
        if self.busy.as_ref().is_some_and(|(p, _)| p == path) {
            self.busy = None;
        }
        let file = match result {
            Ok(f) => f,
            Err(e) => {
                let changed = agent_defs::is_changed_error(&e);
                if changed {
                    self.want_load = true;
                }
                // A prompt that lost the race gets the "changed on disk" choice after the reread.
                if !(changed && part == Part::Body) {
                    self.failure = Some((path.to_path_buf(), e));
                }
                return;
            }
        };
        if self.failure.as_ref().is_some_and(|(p, _)| p == path) {
            self.failure = None;
        }
        if let Some(d) = self.drafts.get_mut(path) {
            match part {
                Part::Body => {
                    d.base_body = file.body.clone();
                    d.base_revision = file.revision.clone();
                    d.disk_revision = None;
                }
                // The prompt on disk is still the one the draft started from: follow the revision.
                Part::Model if file.body == d.base_body => {
                    d.base_revision = file.revision.clone();
                }
                Part::Model => {}
            }
        }
        self.listing = self.listing.take().map(|l| l.replace(file));
    }

    /// A write to omp's agent settings ended (either way): reread them.
    pub fn setting_done(&mut self) {
        self.setting_busy = false;
        self.want_load = true;
    }

    /// A "New agent" or "Customize" ended; `Ok` carries the new file, selected once listed.
    pub fn created(&mut self, result: Result<AgentFile, String>) {
        let customized = self.customizing.take();
        match result {
            Ok(file) => {
                self.create = None;
                self.select_after_load = Some(file.path);
                self.want_load = true;
            }
            Err(e) => match (&mut self.create, customized) {
                (Some(d), _) if d.busy => {
                    d.busy = false;
                    d.error = Some(e);
                }
                (_, Some(builtin)) => self.failure = Some((builtin, e)),
                _ => {}
            },
        }
    }

    /// A delete of `path` ended.
    pub fn deleted(&mut self, path: &Path, result: Result<(), String>) {
        let then = match self.deleting.take() {
            Some((p, then)) if p == path => then,
            other => {
                self.deleting = other;
                None
            }
        };
        match result {
            Ok(()) => {
                self.drafts.remove(path);
                self.select_after_load = then;
                self.want_load = true;
            }
            Err(e) => {
                if agent_defs::is_changed_error(&e) {
                    self.want_load = true;
                }
                self.failure = Some((path.to_path_buf(), e));
            }
        }
    }

    /// Files with unsaved prompt edits.
    pub fn unsaved(&self) -> usize {
        self.drafts.values().filter(|d| d.dirty()).count()
    }
}

// ------------------------------------------------------------------ models

/// What the model chip shows for a `model` value.
struct ChipLabel {
    name: String,
    level: Option<String>,
    tip: String,
    /// The single catalog-style selector the dropdown marks as current.
    current: Option<String>,
}

/// `roles`: omp's model roles, to say what `@smol` stands for.
fn chip_label(
    model: Option<&str>,
    thinking: Option<&str>,
    models: &ModelsCache,
    roles: Option<&AgentSettings>,
) -> ChipLabel {
    let Some(model) = model else {
        return ChipLabel {
            name: "Session model".to_owned(),
            level: thinking.map(str::to_owned),
            tip: "No model set: the agent runs on the model of the session that spawns it".into(),
            current: None,
        };
    };
    if model.contains(',') {
        return ChipLabel {
            name: model.to_owned(),
            level: None,
            tip: format!("Fallback list: omp uses the first available of {model}"),
            current: None,
        };
    }
    if model.starts_with('@') || model.starts_with("pi/") {
        let role = model.trim_start_matches('@').trim_start_matches("pi/");
        let role = role.split_once(':').map_or(role, |(r, _)| r);
        return match roles.and_then(|s| s.role_model(model)) {
            Some(resolved) => {
                let (selector, level) = split_level(resolved);
                let short = models
                    .get(selector)
                    .map_or(selector, |m| short_model_name(&m.name));
                ChipLabel {
                    name: format!("{model} · {short}"),
                    level: split_level(model)
                        .1
                        .or(thinking)
                        .or(level)
                        .map(str::to_owned),
                    tip: format!(
                        "{model} is omp's {role} model role, now {resolved} \
                         (roles are set in Settings → omp → Model)"
                    ),
                    current: None,
                }
            }
            None => ChipLabel {
                name: model.to_owned(),
                level: thinking.map(str::to_owned),
                tip: format!("{model} is an omp model role (Settings → omp → Model)"),
                current: None,
            },
        };
    }
    let (selector, level) = split_level(model);
    let name = models
        .get(selector)
        .map_or(selector, |m| short_model_name(&m.name))
        .to_owned();
    ChipLabel {
        name,
        level: level.or(thinking).map(str::to_owned),
        tip: model.to_owned(),
        current: Some(selector.to_owned()),
    }
}

/// The `model` value a dropdown choice writes, `None` when the action is not about the model.
/// A model picked without a staged level keeps the agent's level when the new model has it.
fn model_for(
    action: &PickerAction,
    current: Option<&str>,
    current_level: Option<&str>,
    models: &ModelsCache,
) -> Option<String> {
    match action {
        PickerAction::Switch { selector, level } => {
            let level = level.clone().or_else(|| {
                models
                    .get(selector)
                    .and_then(|m| model_picker::level_for_switch(m, current_level))
            });
            Some(match level {
                Some(l) => format!("{selector}:{l}"),
                None => selector.clone(),
            })
        }
        PickerAction::Effort(level) => current.map(|s| format!("{s}:{level}")),
        _ => None,
    }
}

// ------------------------------------------------------------------ page

const LIST_MIN_W: f32 = 230.0;
const LIST_MAX_W: f32 = 310.0;
const ROW_H: f32 = 46.0;
const GAP: f32 = 16.0;
const DESCRIPTION_ROWS: usize = 3;

pub fn show(ui: &mut Ui, state: &mut AgentsState, input: &AgentsInput<'_>) -> Vec<AgentsAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();
    let first = state.listing.is_none() && state.load_error.is_none();
    if (state.want_load || first) && !state.loading {
        state.want_load = false;
        state.loading = true;
        actions.push(AgentsAction::Load);
    }

    ui.horizontal(|ui| {
        ui.label(theme::strong("Agents").size(22.0).color(pal.text_strong));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if state.loading {
                ui.add(Spin::new(18.0, pal.accent));
            } else if icon_button(ui, ICON_REFRESH_CW, "Reread the agents", false).clicked() {
                state.want_load = true;
            }
            if state.listing.is_some()
                && DraftButton::new("New agent")
                    .icon(ICON_FILE_PLUS)
                    .show(ui)
                    .on_hover_text("Write a new agent for every session or for one project")
                    .clicked()
            {
                state.create = Some(CreateDialog::new(0));
            }
        });
    });
    ui.add(
        egui::Label::new(
            RichText::new(
                "The task agents omp offers: its built-ins, yours and each project's \
                 .omp/agents. Changes apply to the next task an agent runs, in running \
                 sessions too.",
            )
            .color(pal.weak),
        )
        .wrap_mode(egui::TextWrapMode::Wrap),
    );
    ui.add_space(14.0);

    let Some(listing) = &state.listing else {
        match &state.load_error {
            Some(e) => {
                empty_state(
                    ui,
                    ICON_CIRCLE_ALERT,
                    pal.danger,
                    "Could not read the agents",
                    e,
                );
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    if DraftButton::new("Retry")
                        .icon(ICON_REFRESH_CW)
                        .show(ui)
                        .clicked()
                    {
                        state.want_load = true;
                    }
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.add(Spin::new(16.0, pal.accent));
                    ui.label(RichText::new("Reading agent definitions…").color(pal.weak));
                });
            }
        }
        return actions;
    };
    if let Some(e) = &state.load_error {
        banner(
            ui,
            ICON_CIRCLE_ALERT,
            pal.danger,
            &format!("Reload failed: {e}"),
        );
    }

    let full = ui.available_rect_before_wrap();
    let list_w = (full.width() * 0.32).clamp(LIST_MIN_W, LIST_MAX_W);
    let list_rect = Rect::from_min_size(full.min, vec2(list_w, full.height()));
    let edit_rect = Rect::from_min_max(pos2(full.min.x + list_w + GAP, full.min.y), full.max);
    ui.scope_builder(UiBuilder::new().max_rect(list_rect), |ui| {
        let new_in = list_pane(
            ui,
            listing,
            &mut state.selected,
            &mut state.revealed,
            &mut state.filter,
            &state.drafts,
            &mut actions,
        );
        if let Some(target) = new_in {
            state.create = Some(CreateDialog::new(target));
        }
    });
    ui.scope_builder(UiBuilder::new().max_rect(edit_rect), |ui| {
        let selected = state.selected.clone();
        match selected.as_deref().and_then(|p| listing.file(p)) {
            Some(file) => {
                let parts = EditorParts {
                    drafts: &mut state.drafts,
                    picker: &mut state.picker,
                    busy: &mut state.busy,
                    failure: &mut state.failure,
                    setting_busy: &mut state.setting_busy,
                    customizing: &mut state.customizing,
                    ask_delete: &mut state.ask_delete,
                    selected: &mut state.selected,
                };
                if listing
                    .dir_of(&file.path)
                    .is_some_and(|d| d.scope == Scope::Bundled)
                {
                    builtin_pane(ui, file, listing, parts, input, &mut actions);
                } else {
                    editor_pane(ui, file, listing, parts, input, &mut actions);
                }
            }
            None => {
                card_frame(ui).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    empty_state(
                        ui,
                        ICON_BOT,
                        pal.accent,
                        "No agents",
                        "Create one with New agent, or reread once omp is installed.",
                    );
                });
            }
        }
    });
    ui.allocate_rect(full, Sense::hover());

    // Dialogs.
    let ctx = ui.ctx().clone();
    if let Some(d) = &mut state.create {
        match create::show(&ctx, d, &listing.targets, &listing.catalog) {
            Some(Outcome::Cancel) => state.create = None,
            Some(Outcome::Create { dir, agent }) => {
                actions.push(AgentsAction::Create { dir, agent })
            }
            None => {}
        }
    }
    if let Some(ask) = &state.ask_delete {
        let body = format!("Deletes {}. {}", abbreviate_home(&ask.path), ask.after);
        let confirm = Confirm {
            title: format!("Delete {}?", ask.name),
            body,
            confirm_label: "Delete".to_owned(),
            danger: true,
        };
        match confirm::show(&ctx, "agent-delete", &confirm) {
            Some(true) => {
                if let Some(ask) = state.ask_delete.take() {
                    state.deleting = Some((ask.path.clone(), ask.then));
                    actions.push(AgentsAction::Delete {
                        path: ask.path,
                        revision: ask.revision,
                        name: ask.name,
                    });
                }
            }
            Some(false) => state.ask_delete = None,
            None => {}
        }
    }
    actions
}

/// The mutable parts of the state the editor uses (the listing stays borrowed for reading).
struct EditorParts<'s> {
    drafts: &'s mut HashMap<PathBuf, Draft>,
    picker: &'s mut PickerState,
    busy: &'s mut Option<(PathBuf, Part)>,
    failure: &'s mut Option<(PathBuf, String)>,
    setting_busy: &'s mut bool,
    customizing: &'s mut Option<PathBuf>,
    ask_delete: &'s mut Option<DeleteAsk>,
    selected: &'s mut Option<PathBuf>,
}

// ------------------------------------------------------------------ list

/// Returns the target a group's "+" asked to create an agent in.
fn list_pane(
    ui: &mut Ui,
    listing: &Listing,
    selected: &mut Option<PathBuf>,
    revealed: &mut Option<PathBuf>,
    filter: &mut String,
    drafts: &HashMap<PathBuf, Draft>,
    actions: &mut Vec<AgentsAction>,
) -> Option<usize> {
    let pal = Palette::of_ui(ui);
    let mut new_in = None;
    card_frame(ui)
        .inner_margin(Margin::symmetric(10, 12))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.add(
                TextEdit::singleline(filter)
                    .prefix(icon_text(ICON_SEARCH, 15.0, pal.weak))
                    .hint_text("Filter agents…")
                    .desired_width(ui.available_width())
                    .margin(vec2(8.0, 6.0)),
            );
            ui.add_space(6.0);
            let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
            egui::ScrollArea::vertical()
                .id_salt("agent-list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (di, dir) in listing.catalog.dirs.iter().enumerate() {
                        let shown: Vec<&ListEntry> = listing
                            .entries
                            .iter()
                            .filter(|e| e.dir == di)
                            .filter(|e| words.iter().all(|w| e.haystack.contains(w.as_str())))
                            .collect();
                        if !words.is_empty() && shown.is_empty() {
                            continue;
                        }
                        let target = listing.targets.iter().position(|t| t.dir == dir.dir);
                        if dir_header(ui, dir, target.is_some(), actions) {
                            new_in = target;
                        }
                        if let Some(e) = &dir.error {
                            let e = match dir.scope {
                                Scope::Bundled => format!("omp did not list them: {e}"),
                                _ => e.clone(),
                            };
                            ui.add(
                                egui::Label::new(RichText::new(e).small().color(pal.danger))
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        } else if dir.agents.is_empty() {
                            ui.add(
                                egui::Label::new(
                                    RichText::new("No agents here yet").small().color(pal.weak),
                                )
                                .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        }
                        for e in shown {
                            let file = &dir.agents[e.idx];
                            let marks = RowMarks {
                                selected: selected.as_deref() == Some(file.path.as_path()),
                                dirty: drafts.get(&file.path).is_some_and(Draft::dirty),
                                disabled: listing.settings().is_some_and(|s| {
                                    file.loaded_name().is_some_and(|n| s.is_disabled(n))
                                }),
                                overridden: listing.settings().and_then(|s| {
                                    file.loaded_name().and_then(|n| s.override_for(n))
                                }),
                            };
                            let resp = agent_row(ui, e, file, &marks);
                            if resp.clicked() {
                                *selected = Some(file.path.clone());
                            } else if marks.selected && *revealed != *selected {
                                resp.scroll_to_me(Some(Align::Center));
                            }
                            if marks.selected || resp.clicked() {
                                revealed.clone_from(selected);
                            }
                        }
                        ui.add_space(10.0);
                    }
                });
        });
    new_in
}

/// A group's caption and path; true when its "+" (new agent here) was clicked.
fn dir_header(
    ui: &mut Ui,
    dir: &AgentDir,
    can_create: bool,
    actions: &mut Vec<AgentsAction>,
) -> bool {
    let pal = Palette::of_ui(ui);
    let icon = match dir.scope {
        Scope::User => ICON_USER,
        Scope::Project => ICON_FOLDER,
        Scope::Bundled => ICON_PACKAGE,
    };
    let mut create = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(icon_text(icon, 14.0, pal.glyph));
        ui.label(theme::section_caption(&scope_title(dir), pal.weak));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if can_create {
                create = icon_button(ui, ICON_PLUS, "New agent here", false).clicked();
            }
            if dir.scope != Scope::Bundled
                && dir.dir.is_dir()
                && icon_button(ui, ICON_FOLDER_OPEN, "Open this folder", false).clicked()
            {
                actions.push(AgentsAction::OpenFolder(dir.dir.clone()));
            }
        });
    });
    let (path, tip) = match dir.scope {
        Scope::Bundled => (
            "omp's own · read-only".to_owned(),
            "Shipped with omp. A user or project agent of the same name replaces one; \
             Customize copies it there to edit it."
                .to_owned(),
        ),
        _ => (abbreviate_home(&dir.dir), dir.dir.display().to_string()),
    };
    ui.add(
        egui::Label::new(
            RichText::new(path)
                .font(theme::label_font(11.0))
                .color(pal.weak),
        )
        .truncate(),
    )
    .on_hover_text(tip);
    ui.add_space(2.0);
    create
}

/// What a list row marks besides its name and model.
struct RowMarks {
    selected: bool,
    dirty: bool,
    disabled: bool,
    overridden: Option<String>,
}

fn agent_row(ui: &mut Ui, entry: &ListEntry, file: &AgentFile, marks: &RowMarks) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let neutral = theme::flat();
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let ctx = ui.ctx();
    let hov = ctx.animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), marks.selected, 0.18);
    let p = ui.painter();
    let radius = if neutral { 6 } else { 10 };
    p.rect_filled(
        rect,
        radius,
        theme::hover_fill(pal).gamma_multiply(0.7 * hov * (1.0 - sel)),
    );
    if sel > 0.01 {
        p.rect(
            rect,
            radius,
            theme::mix(pal.surface, pal.accent, if neutral { 0.10 } else { 0.20 })
                .gamma_multiply(sel),
            Stroke::new(1.0, pal.accent.gamma_multiply(0.5 * sel)),
            StrokeKind::Inside,
        );
    }
    // Status glyphs at the right edge, right to left.
    let mut right = rect.right() - 10.0;
    let mut glyph = |icon: Icon, color: Color32| {
        paint_icon(
            p,
            pos2(right, rect.center().y),
            Align2::RIGHT_CENTER,
            icon,
            14.0,
            color,
        );
        right -= 20.0;
    };
    if file.issue.is_some() {
        glyph(ICON_TRIANGLE_ALERT, pal.warn);
    }
    if marks.disabled {
        glyph(ICON_CIRCLE_PAUSE, pal.weak);
    }
    if marks.overridden.is_some() {
        glyph(ICON_INFO, pal.accent_b);
    }
    if entry.replaced_by.is_some() {
        glyph(ICON_FILE_PEN_LINE, pal.weak);
    }
    if marks.dirty {
        p.circle_filled(pos2(right - 3.0, rect.center().y), 4.0, pal.accent);
        right -= 14.0;
    }
    let text_w = (right - rect.left() - 12.0).max(10.0);
    let replaced_everywhere = entry.replaced_by.as_ref().is_some_and(|(_, all)| *all);
    let name_color = if file.issue.is_some() || marks.disabled || replaced_everywhere {
        pal.weak
    } else {
        theme::mix(pal.text, pal.text_strong, sel)
    };
    let name = p.layout_no_wrap(entry.name.clone(), theme::font(13.5), name_color);
    let name = fit(p, name, &entry.name, theme::font(13.5), name_color, text_w);
    p.galley(pos2(rect.left() + 12.0, rect.top() + 7.0), name, name_color);
    let sub = match &marks.overridden {
        Some(o) => o.clone(),
        None => file
            .model
            .clone()
            .unwrap_or_else(|| "session model".to_owned()),
    };
    let sub_font = theme::label_font(11.0);
    let sub_g = fit(
        p,
        p.layout_no_wrap(sub.clone(), sub_font.clone(), pal.weak),
        &sub,
        sub_font,
        pal.weak,
        text_w,
    );
    p.galley(
        pos2(rect.left() + 12.0, rect.bottom() - 19.0),
        sub_g,
        pal.weak,
    );
    let mut tip = if agent_defs::is_bundled(&file.path) {
        "omp's built-in agent".to_owned()
    } else {
        file.path.display().to_string()
    };
    if let Some(i) = &file.issue {
        tip.push_str(&format!("\nomp skips this file: {}", i.describe()));
    }
    if marks.disabled {
        tip.push_str("\nDisabled in omp (task.disabledAgents)");
    }
    if let Some(o) = &marks.overridden {
        tip.push_str(&format!("\nModel overridden in omp's config: {o}"));
    }
    if let Some(r) = &entry.relation {
        tip.push('\n');
        tip.push_str(r);
    }
    if marks.dirty {
        tip.push_str("\nUnsaved prompt edits");
    }
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// `galley` as is when it fits `max_w`, else `text` ellipsized to fit.
fn fit(
    p: &egui::Painter,
    galley: std::sync::Arc<egui::Galley>,
    text: &str,
    font: egui::FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    if galley.size().x <= max_w {
        return galley;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    p.layout_job(job)
}

// ------------------------------------------------------------------ editor

fn editor_pane(
    ui: &mut Ui,
    file: &AgentFile,
    listing: &Listing,
    parts: EditorParts<'_>,
    input: &AgentsInput<'_>,
    actions: &mut Vec<AgentsAction>,
) {
    let pal = Palette::of_ui(ui);
    let EditorParts {
        drafts,
        picker,
        busy,
        failure,
        setting_busy,
        ask_delete,
        ..
    } = parts;
    let busy_part = busy
        .as_ref()
        .filter(|(p, _)| *p == file.path)
        .map(|(_, part)| *part);
    let settings = listing.settings();
    let name = file.loaded_name();
    let disabled = settings.is_some_and(|s| name.is_some_and(|n| s.is_disabled(n)));
    let overridden = settings.and_then(|s| name.and_then(|n| s.override_for(n)));
    let scope = listing.dir_of(&file.path);

    card_frame(ui).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        pane_header(
            ui,
            ICON_BOT,
            &file.display_name(),
            |ui| {
                if let Some(d) = scope {
                    let (icon, hue) = match d.scope {
                        Scope::User => (ICON_USER, Hue::Blue),
                        _ => (ICON_FOLDER, Hue::Teal),
                    };
                    badge::show(ui, Some(icon), &scope_title(d), hue, 11.5);
                }
                if disabled {
                    badge::show(ui, Some(ICON_CIRCLE_PAUSE), "disabled", Hue::Amber, 11.5)
                        .on_hover_text("Listed in task.disabledAgents: omp does not offer it");
                }
                if let Some(r) = listing
                    .entry(&file.path)
                    .and_then(|e| e.relation.as_deref())
                {
                    badge::show(
                        ui,
                        Some(ICON_INFO),
                        "same name elsewhere",
                        Hue::Violet,
                        11.5,
                    )
                    .on_hover_text(r);
                }
            },
            |ui| {
                if icon_button(ui, ICON_COPY, "Copy the file path", false).clicked() {
                    actions.push(AgentsAction::CopyPath(file.path.display().to_string()));
                }
                if let Some(dir) = file.path.parent()
                    && icon_button(ui, ICON_FOLDER_OPEN, "Open the folder", false).clicked()
                {
                    actions.push(AgentsAction::OpenFolder(dir.to_path_buf()));
                }
                if icon_button(ui, ICON_EXTERNAL_LINK, "Open in the default editor", false)
                    .clicked()
                {
                    actions.push(AgentsAction::OpenFile(file.path.clone()));
                }
                if busy_part.is_none()
                    && icon_button(ui, ICON_TRASH_2, "Delete this agent…", false).clicked()
                {
                    *ask_delete = Some(delete_ask(file, listing));
                }
            },
        );
        ui.add_space(6.0);
        if let Some(d) = &file.description {
            description(ui, d);
        }
        ui.add(
            egui::Label::new(
                RichText::new(abbreviate_home(&file.path))
                    .font(theme::label_font(11.0))
                    .color(pal.weak),
            )
            .truncate(),
        )
        .on_hover_text(file.path.display().to_string());
        ui.add_space(8.0);
        if let Some(issue) = &file.issue {
            banner(
                ui,
                ICON_TRIANGLE_ALERT,
                pal.warn,
                &format!("omp skips this file: {}.", issue.describe()),
            );
        }
        if let Some((_, e)) = failure.as_ref().filter(|(p, _)| *p == file.path)
            && banner_dismiss(
                ui,
                ICON_CIRCLE_ALERT,
                pal.danger,
                &format!("Not saved: {e}"),
            )
        {
            *failure = None;
        }
        if let Some(n) = name {
            enabled_row(ui, n, settings, setting_busy, actions);
        }

        // Model.
        rule(ui);
        ui.label(theme::section_caption("Model", pal.weak));
        ui.add_space(4.0);
        model_row(ui, file, listing, busy_part, picker, input, busy, actions);
        if let (Some(o), Some(n), Some(s)) = (&overridden, name, settings) {
            let mut remove = false;
            banner_with(
                ui,
                ICON_INFO,
                pal.accent_b,
                &format!(
                    "omp's config overrides this agent's model with {o} \
                     (task.agentModelOverrides), so the model above is not used."
                ),
                |ui| {
                    remove = DraftButton::new("Remove override")
                        .enabled(!*setting_busy)
                        .show(ui)
                        .clicked();
                },
            );
            if remove {
                *setting_busy = true;
                actions.push(AgentsAction::SetSetting {
                    key: OVERRIDES_KEY,
                    value: s.overrides_with(n, None),
                    done: format!("Removed the model override of {n}"),
                });
            }
        }
        settings_unknown(ui, listing);

        // System prompt.
        rule(ui);
        prompt_section(ui, file, drafts, busy_part, busy, actions);
    });
}

/// The confirmation a delete of `file` asks for: what applies once it is gone.
fn delete_ask(file: &AgentFile, listing: &Listing) -> DeleteAsk {
    let name = file.display_name();
    let project = listing
        .dir_of(&file.path)
        .is_some_and(|d| d.scope == Scope::Project);
    let user = project
        .then(|| {
            file.loaded_name()
                .and_then(|n| listing.declaring(Scope::User, n))
        })
        .flatten();
    let builtin = file
        .loaded_name()
        .and_then(|n| listing.declaring(Scope::Bundled, n));
    let (then, after) = match (user, builtin) {
        (Some(u), _) => (
            Some(u.path.clone()),
            format!("Your user agent {name} applies again in this project."),
        ),
        (None, Some(b)) => (
            Some(b.path.clone()),
            format!("omp's built-in {name} applies again."),
        ),
        (None, None) => (None, "Sessions can no longer use this agent.".to_owned()),
    };
    DeleteAsk {
        path: file.path.clone(),
        revision: file.revision.clone(),
        name,
        after,
        then,
    }
}

/// omp's built-in: read-only, model overridden in omp's config, customized by copying.
fn builtin_pane(
    ui: &mut Ui,
    file: &AgentFile,
    listing: &Listing,
    parts: EditorParts<'_>,
    input: &AgentsInput<'_>,
    actions: &mut Vec<AgentsAction>,
) {
    let pal = Palette::of_ui(ui);
    let EditorParts {
        picker,
        failure,
        setting_busy,
        customizing,
        selected,
        ..
    } = parts;
    let settings = listing.settings();
    let name = file.loaded_name();
    let disabled = settings.is_some_and(|s| name.is_some_and(|n| s.is_disabled(n)));
    let entry = listing.entry(&file.path);

    card_frame(ui).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        pane_header(
            ui,
            ICON_PACKAGE,
            &file.display_name(),
            |ui| {
                badge::show(ui, Some(ICON_PACKAGE), "Built-in", Hue::Violet, 11.5)
                    .on_hover_text("Shipped with omp: read-only");
                if disabled {
                    badge::show(ui, Some(ICON_CIRCLE_PAUSE), "disabled", Hue::Amber, 11.5)
                        .on_hover_text("Listed in task.disabledAgents: omp does not offer it");
                }
            },
            |ui| {
                if let Some(n) = name {
                    customize_menu(ui, file, n, listing, customizing, actions);
                }
            },
        );
        ui.add_space(6.0);
        if let Some(d) = &file.description {
            description(ui, d);
        }
        ui.add_space(8.0);
        if let Some(e) = entry
            && let Some((by, everywhere)) = &e.replaced_by
        {
            let text = if *everywhere {
                format!(
                    "Your user agent {} replaces this built-in in every session: edit that \
                     file, or delete it to come back to this one.",
                    by.file_name().unwrap_or_default().to_string_lossy()
                )
            } else {
                format!("{}.", e.relation.as_deref().unwrap_or_default())
            };
            let mut show_it = false;
            banner_with(ui, ICON_INFO, pal.accent_b, &text, |ui| {
                show_it = DraftButton::new("Show it").show(ui).clicked();
            });
            if show_it {
                *selected = Some(by.clone());
            }
        }
        if let Some((_, e)) = failure.as_ref().filter(|(p, _)| *p == file.path)
            && banner_dismiss(
                ui,
                ICON_CIRCLE_ALERT,
                pal.danger,
                &format!("Not customized: {e}"),
            )
        {
            *failure = None;
        }
        if let Some(n) = name {
            enabled_row(ui, n, settings, setting_busy, actions);
        }

        // Model: an override in omp's config.
        rule(ui);
        ui.label(theme::section_caption("Model", pal.weak));
        ui.add_space(4.0);
        if let Some(n) = name {
            builtin_model_row(ui, file, n, settings, picker, input, setting_busy, actions);
        }
        settings_unknown(ui, listing);

        // System prompt: read-only.
        rule(ui);
        ui.horizontal(|ui| {
            ui.label(theme::section_caption("System prompt", pal.weak));
            ui.label(RichText::new("read-only").small().color(pal.weak));
        });
        ui.label(
            RichText::new("Customize to edit it: the copy replaces the built-in")
                .small()
                .color(pal.weak),
        );
        ui.add_space(4.0);
        let editor_h = (ui.available_height() - 4.0).max(120.0);
        egui::ScrollArea::vertical()
            .id_salt(("agent-prompt", &file.path))
            .max_height(editor_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    TextEdit::multiline(&mut file.body.as_str())
                        .id(egui::Id::unique(("agent-prompt-view", &file.path)))
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .min_size(vec2(0.0, editor_h))
                        .margin(Margin::symmetric(10, 8)),
                );
            });
    });
}

/// "Customize ▾": copy the built-in into the user's agents or a project's.
fn customize_menu(
    ui: &mut Ui,
    file: &AgentFile,
    name: &str,
    listing: &Listing,
    customizing: &mut Option<PathBuf>,
    actions: &mut Vec<AgentsAction>,
) {
    let pal = Palette::of_ui(ui);
    if customizing.as_deref() == Some(file.path.as_path()) {
        ui.add(Spin::new(18.0, pal.accent));
        return;
    }
    let resp = ui.add(
        egui::Button::new((
            icon_text(ICON_FILE_PEN_LINE, 15.0, pal.glyph),
            RichText::new("Customize").color(pal.text),
            icon_text(ICON_CHEVRON_DOWN, 14.0, pal.weak),
        ))
        .min_size(vec2(0.0, 30.0))
        .corner_radius(theme::rad(6, 10)),
    );
    let resp = resp
        .on_hover_text("Copy it into your agents or a project's, where the copy replaces it")
        .on_hover_cursor(CursorIcon::PointingHand);
    theme::dropdown_menu(&resp, 300.0, |ui| {
        for t in &listing.targets {
            let (icon, label) = match t.scope {
                Scope::User => (ICON_USER, "As your user agent".to_owned()),
                _ => (ICON_FOLDER, format!("In project {}", t.title)),
            };
            let taken = create::existing(&listing.catalog, &t.dir, name);
            let r = menu_item(ui, icon, &label, None, taken.is_none(), false);
            let r = match taken {
                Some(f) => r.on_disabled_hover_text(format!(
                    "{} already defines {name} there",
                    abbreviate_home(&f.path)
                )),
                None => r.on_hover_text(abbreviate_home(&t.dir.join(format!("{name}.md")))),
            };
            if r.clicked() {
                *customizing = Some(file.path.clone());
                actions.push(AgentsAction::Customize {
                    dir: t.dir.clone(),
                    name: name.to_owned(),
                });
                ui.close();
            }
        }
    });
}

fn pane_header(
    ui: &mut Ui,
    icon: Icon,
    title: &str,
    badges: impl FnOnce(&mut Ui),
    buttons: impl FnOnce(&mut Ui),
) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        crate::card::icon_tile(ui, 34.0, icon, pal.accent, false);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(theme::strong(title).size(19.0).color(pal.text_strong));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                badges(ui);
            });
        });
        ui.with_layout(Layout::right_to_left(Align::Center), buttons);
    });
}

/// The on/off switch of `task.disabledAgents` for `name` (nothing while the settings are
/// unknown).
fn enabled_row(
    ui: &mut Ui,
    name: &str,
    settings: Option<&AgentSettings>,
    setting_busy: &mut bool,
    actions: &mut Vec<AgentsAction>,
) {
    let Some(s) = settings else {
        return;
    };
    let pal = Palette::of_ui(ui);
    let enabled = !s.is_disabled(name);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let mut on = enabled;
        ui.add_enabled_ui(!*setting_busy, |ui| toggle(ui, &mut on))
            .inner
            .on_hover_text(format!(
                "task.disabledAgents in omp's config: applies to every agent named {name}"
            ));
        let (text, color) = if enabled {
            ("Enabled: sessions can spawn it", pal.text)
        } else {
            ("Disabled: omp does not offer it to sessions", pal.warn)
        };
        ui.label(RichText::new(text).color(color));
        if *setting_busy {
            ui.add(Spin::new(16.0, pal.accent));
        }
        if on != enabled {
            *setting_busy = true;
            actions.push(AgentsAction::SetSetting {
                key: DISABLED_KEY,
                value: s.disabled_with(name, !on),
                done: format!("{} {name}", if on { "Enabled" } else { "Disabled" }),
            });
        }
    });
    ui.add_space(4.0);
}

fn settings_unknown(ui: &mut Ui, listing: &Listing) {
    if let Some(Err(e)) = &listing.catalog.settings {
        let pal = Palette::of_ui(ui);
        ui.label(
            RichText::new(format!("omp's agent settings are unknown: {e}"))
                .small()
                .color(pal.weak),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn model_row(
    ui: &mut Ui,
    file: &AgentFile,
    listing: &Listing,
    busy_part: Option<Part>,
    picker: &mut PickerState,
    input: &AgentsInput<'_>,
    busy: &mut Option<(PathBuf, Part)>,
    actions: &mut Vec<AgentsAction>,
) {
    let pal = Palette::of_ui(ui);
    let label = chip_label(
        file.model.as_deref(),
        file.thinking_level.as_deref(),
        input.models,
        listing.settings(),
    );
    let editable = file.frontmatter && busy_part.is_none();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let chip = ui
            .add_enabled_ui(editable, |ui| model_chip(ui, &label, file.model.is_some()))
            .inner;
        let chip = chip.on_hover_text(&label.tip);
        if busy_part == Some(Part::Model) {
            ui.add(Spin::new(16.0, pal.accent));
        }
        let current_level = file
            .model
            .as_deref()
            .and_then(|m| split_level(m).1)
            .or(file.thinking_level.as_deref());
        let mut edit = pick_model(
            &chip,
            picker,
            input,
            &label,
            current_level,
            "Pick a model first; the agent runs on the session's model until then",
            actions,
        )
        .filter(|m| file.model.as_deref() != Some(m.as_str()))
        .map(Some);
        if file.model.is_some()
            && editable
            && icon_button(
                ui,
                ICON_X,
                "Remove the model: run on the session's model",
                false,
            )
            .clicked()
        {
            edit = Some(None);
        }
        if let Some(model) = edit {
            *busy = Some((file.path.clone(), Part::Model));
            actions.push(AgentsAction::Edit {
                path: file.path.clone(),
                revision: file.revision.clone(),
                edit: Edit::Model(model),
            });
        }
    });
    let hint = if !file.frontmatter {
        "Add frontmatter (name, description) to set a model".to_owned()
    } else {
        match (&file.model, &file.thinking_level) {
            (Some(m), Some(t)) if split_level(m).1.is_none() => {
                format!("Written to the file's model field · thinking {t} from thinkingLevel")
            }
            (None, _) => "Unset: the agent runs on the model of the session that spawns it".into(),
            _ => "Written to the file's model field".into(),
        }
    };
    ui.label(RichText::new(hint).small().color(pal.weak));
}

/// A built-in's model: omp's default, or the override in `task.agentModelOverrides` the
/// dropdown writes.
#[allow(clippy::too_many_arguments)]
fn builtin_model_row(
    ui: &mut Ui,
    file: &AgentFile,
    name: &str,
    settings: Option<&AgentSettings>,
    picker: &mut PickerState,
    input: &AgentsInput<'_>,
    setting_busy: &mut bool,
    actions: &mut Vec<AgentsAction>,
) {
    let pal = Palette::of_ui(ui);
    let overridden = settings.and_then(|s| s.override_for(name));
    let effective = overridden.as_deref().or(file.model.as_deref());
    let label = chip_label(
        effective,
        file.thinking_level.as_deref(),
        input.models,
        settings,
    );
    let editable = settings.is_some() && !*setting_busy;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let chip = ui
            .add_enabled_ui(editable, |ui| model_chip(ui, &label, effective.is_some()))
            .inner;
        let chip = chip.on_hover_text(&label.tip);
        let current_level = effective
            .and_then(|m| split_level(m).1)
            .or(file.thinking_level.as_deref());
        let mut write = pick_model(
            &chip,
            picker,
            input,
            &label,
            current_level,
            "omp's default is a role or a list: pick a model to override it",
            actions,
        )
        .filter(|m| overridden.as_deref() != Some(m.as_str()))
        .map(Some);
        if overridden.is_some()
            && editable
            && icon_button(ui, ICON_UNDO_2, "Back to omp's default model", false).clicked()
        {
            write = Some(None);
        }
        if *setting_busy {
            ui.add(Spin::new(16.0, pal.accent));
        }
        if let (Some(model), Some(s)) = (write, settings) {
            *setting_busy = true;
            actions.push(AgentsAction::SetSetting {
                key: OVERRIDES_KEY,
                value: s.overrides_with(name, model.as_deref()),
                done: match model {
                    Some(m) => format!("{name} now runs on {m}"),
                    None => format!("{name} is back on omp's default model"),
                },
            });
        }
    });
    let default = file.model.as_deref().unwrap_or("the session's model");
    let hint = match overridden {
        Some(_) => format!(
            "Overridden in omp's config (task.agentModelOverrides) · omp's default: {default}"
        ),
        None => "omp's default · picking a model overrides it in omp's config".to_owned(),
    };
    ui.label(RichText::new(hint).small().color(pal.weak));
}

/// Runs the model dropdown under `chip`; returns the `model` value a pick asks for and forwards
/// the dropdown's own controls.
fn pick_model(
    chip: &egui::Response,
    picker: &mut PickerState,
    input: &AgentsInput<'_>,
    label: &ChipLabel,
    current_level: Option<&str>,
    unset: &str,
    actions: &mut Vec<AgentsAction>,
) -> Option<String> {
    let picked = model_picker::show(
        chip,
        picker,
        &PickerInput {
            catalog: input.models,
            current: label.current.as_deref(),
            level: label.level.as_deref(),
            favorites: input.favorites,
            provider: input.provider,
            unset,
        },
    );
    let mut model = None;
    for a in picked {
        match a {
            PickerAction::Switch { .. } | PickerAction::Effort(_) => {
                if let Some(m) =
                    model_for(&a, label.current.as_deref(), current_level, input.models)
                {
                    model = Some(m);
                }
            }
            PickerAction::Refresh => actions.push(AgentsAction::ReloadModels),
            PickerAction::ToggleFavorite(s) => actions.push(AgentsAction::ToggleFavorite(s)),
            PickerAction::SetProvider(p) => actions.push(AgentsAction::ModelProvider(p)),
        }
    }
    model
}

fn model_chip(ui: &mut Ui, label: &ChipLabel, set: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let name =
        RichText::new(&label.name)
            .size(14.5)
            .color(if set { pal.text_strong } else { pal.weak });
    let mut atoms: Vec<egui::Atom<'_>> = vec![icon_text(ICON_BOT, 15.0, pal.glyph).into()];
    atoms.push(name.into());
    if let Some(l) = &label.level {
        atoms.push(
            RichText::new(thinking_short(l))
                .size(13.0)
                .color(pal.accent)
                .into(),
        );
    }
    atoms.push(icon_text(ICON_CHEVRONS_UP_DOWN, 15.0, pal.weak).into());
    ui.add(
        egui::Button::new(atoms.into_iter().collect::<egui::Atoms<'_>>())
            .min_size(vec2(260.0, 34.0))
            .corner_radius(theme::rad(6, 10)),
    )
    .on_hover_cursor(CursorIcon::PointingHand)
}

fn prompt_section(
    ui: &mut Ui,
    file: &AgentFile,
    drafts: &mut HashMap<PathBuf, Draft>,
    busy_part: Option<Part>,
    busy: &mut Option<(PathBuf, Part)>,
    actions: &mut Vec<AgentsAction>,
) {
    let pal = Palette::of_ui(ui);
    let draft = drafts
        .entry(file.path.clone())
        .or_insert_with(|| Draft::of(file));
    let dirty = draft.dirty();
    let saving = busy_part == Some(Part::Body);
    let mut save = false;
    let mut revert = false;
    ui.horizontal(|ui| {
        ui.label(theme::section_caption("System prompt", pal.weak));
        if dirty {
            ui.label(RichText::new("● unsaved").small().color(pal.accent));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            save = DraftButton::new("Save")
                .kind(DraftKind::Primary)
                .icon(ICON_SAVE)
                .enabled(dirty && !saving && draft.disk_revision.is_none())
                .show(ui)
                .on_hover_text("Save the prompt to the file (Ctrl+S)")
                .clicked();
            revert = DraftButton::new("Revert")
                .icon(ICON_UNDO_2)
                .enabled(dirty && !saving)
                .show(ui)
                .on_hover_text("Discard the unsaved edits")
                .clicked();
            if saving {
                ui.add(Spin::new(16.0, pal.accent));
            }
        });
    });
    ui.add_space(4.0);
    if let Some(disk) = draft.disk_revision.clone() {
        let (mut keep, mut take) = (false, false);
        banner_with(
            ui,
            ICON_TRIANGLE_ALERT,
            pal.warn,
            "The file changed on disk while you were editing.",
            |ui| {
                take = DraftButton::new("Use the file").show(ui).clicked();
                keep = DraftButton::new("Keep my edits")
                    .show(ui)
                    .on_hover_text(
                        "Saving then replaces the prompt on disk; the frontmatter on disk stays",
                    )
                    .clicked();
            },
        );
        if keep {
            draft.base_revision = disk;
            draft.base_body = file.body.clone();
            draft.disk_revision = None;
        } else if take {
            *draft = Draft::of(file);
        }
    }
    let editor_h = (ui.available_height() - 4.0).max(120.0);
    egui::ScrollArea::vertical()
        .id_salt(("agent-prompt", &file.path))
        .max_height(editor_h)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_enabled(
                !saving,
                TextEdit::multiline(&mut draft.text)
                    .id(egui::Id::unique(("agent-prompt-edit", &file.path)))
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .min_size(vec2(0.0, editor_h))
                    .margin(Margin::symmetric(10, 8)),
            );
        });
    let shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
    if dirty && !saving && draft.disk_revision.is_none() {
        save |= ui.input_mut(|i| i.consume_shortcut(&shortcut));
    }
    if revert {
        *draft = Draft::of(file);
    } else if save {
        *busy = Some((file.path.clone(), Part::Body));
        actions.push(AgentsAction::Edit {
            path: file.path.clone(),
            revision: draft.base_revision.clone(),
            edit: Edit::Body(draft.text.trim().to_owned()),
        });
    }
}

// ------------------------------------------------------------------ chrome

fn rule(ui: &mut Ui) {
    ui.add_space(8.0);
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    crate::card::header_rule(ui, r.left(), r.right(), r.center().y, Color32::TRANSPARENT);
    ui.add_space(8.0);
}

/// The agent's description, at most [`DESCRIPTION_ROWS`] rows (all of it in the tooltip when
/// cut): it is what the parent model reads to pick the agent, and can be long.
fn description(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat::simple(theme::font(13.0), pal.text),
    );
    job.wrap = egui::text::TextWrapping {
        max_width: ui.available_width(),
        max_rows: DESCRIPTION_ROWS,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    let galley = ui.painter().layout_job(job);
    let (rect, resp) = ui.allocate_exact_size(galley.size(), Sense::hover());
    let elided = galley.elided;
    ui.painter().galley(rect.min, galley, pal.text);
    if elided {
        resp.on_hover_ui(|ui| {
            ui.set_max_width(480.0);
            ui.label(text);
        });
    }
}

fn banner(ui: &mut Ui, icon: Icon, tone: Color32, text: &str) {
    banner_frame(ui, tone, |ui| banner_text(ui, icon, tone, text, 0.0));
}

/// A notice with a dismiss button at its right; true when it was clicked.
fn banner_dismiss(ui: &mut Ui, icon: Icon, tone: Color32, text: &str) -> bool {
    let mut dismissed = false;
    banner_frame(ui, tone, |ui| {
        ui.horizontal_top(|ui| {
            let side = theme::dens(28.0, 32.0);
            banner_text(ui, icon, tone, text, side + 8.0);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                dismissed = icon_button(ui, ICON_X, "Dismiss", false).clicked();
            });
        });
    });
    dismissed
}

/// A notice with `buttons` right-aligned under its text.
fn banner_with(ui: &mut Ui, icon: Icon, tone: Color32, text: &str, buttons: impl FnOnce(&mut Ui)) {
    banner_frame(ui, tone, |ui| {
        banner_text(ui, icon, tone, text, 0.0);
        ui.add_space(6.0);
        ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), buttons));
    });
}

fn banner_frame(ui: &mut Ui, tone: Color32, add: impl FnOnce(&mut Ui)) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(theme::accent_wash(pal.surface, tone, 0.08))
        .stroke(Stroke::new(1.0, tone.gamma_multiply(0.35)))
        .corner_radius(theme::rad(8, 10))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(8.0);
}

/// Icon and text wrapped to the width left after `reserve` points.
fn banner_text(ui: &mut Ui, icon: Icon, tone: Color32, text: &str, reserve: f32) {
    let pal = Palette::of_ui(ui);
    let width = (ui.available_width() - reserve).max(80.0);
    ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::left_to_right(Align::Min), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(icon_text(icon, 16.0, tone));
        ui.add(
            egui::Label::new(RichText::new(text).small().color(pal.text))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
}

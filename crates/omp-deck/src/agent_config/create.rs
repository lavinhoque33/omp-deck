//! Where new agents can go ([`Target`]) and the "New agent" dialog.

use std::path::{Path, PathBuf};

use deck_core::agent_defs::{
    self, AGENTS_DIR, AgentFile, Catalog, NewAgent, PROJECT_CONFIG_DIR, Scope,
};
use deck_core::sessions::abbreviate_home;
use egui::{Context, Margin, RichText, TextEdit, Ui};

use crate::confirm::{Dialog, show_dialog};
use crate::icons::ICON_FILE_PLUS;
use crate::theme::Palette;

/// A directory a new (or customized) agent can be written to.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Target {
    pub scope: Scope,
    /// `User` / the project folder's name.
    pub title: String,
    /// The `agents` directory (a project's may not exist yet).
    pub dir: PathBuf,
}

impl Target {
    /// "User" / "Project omp-deck".
    pub fn label(&self) -> String {
        match self.scope {
            Scope::Project => format!("Project {}", self.title),
            _ => self.title.clone(),
        }
    }
}

/// The user directory, then every project folder (with an `.omp/agents` or not), by path.
pub(super) fn targets(catalog: &Catalog) -> Vec<Target> {
    let mut out: Vec<Target> = catalog
        .dirs
        .iter()
        .filter(|d| d.scope == Scope::User)
        .map(|d| Target {
            scope: Scope::User,
            title: "User".to_owned(),
            dir: d.dir.clone(),
        })
        .collect();
    let mut projects: Vec<(PathBuf, PathBuf)> = catalog
        .dirs
        .iter()
        .filter(|d| d.scope == Scope::Project)
        .map(|d| (d.root.clone(), d.dir.clone()))
        .chain(
            catalog
                .bare_roots
                .iter()
                .map(|r| (r.clone(), r.join(PROJECT_CONFIG_DIR).join(AGENTS_DIR))),
        )
        .collect();
    projects.sort();
    out.extend(projects.into_iter().map(|(root, dir)| Target {
        scope: Scope::Project,
        title: root.file_name().map_or_else(
            || abbreviate_home(&root),
            |n| n.to_string_lossy().into_owned(),
        ),
        dir,
    }));
    out
}

/// The file in `dir` that already declares `name` (omp would load only one of them).
pub(super) fn existing<'c>(catalog: &'c Catalog, dir: &Path, name: &str) -> Option<&'c AgentFile> {
    catalog
        .dirs
        .iter()
        .filter(|d| d.scope != Scope::Bundled && d.dir == dir)
        .flat_map(|d| &d.agents)
        .find(|a| a.loaded_name() == Some(name))
}

/// The "New agent" form.
#[derive(Debug, Default)]
pub(super) struct CreateDialog {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Index into the page's [`targets`].
    pub target: usize,
    /// The last failed attempt.
    pub error: Option<String>,
    /// Created, waiting for the reply.
    pub busy: bool,
    focused: bool,
}

impl CreateDialog {
    pub fn new(target: usize) -> Self {
        CreateDialog {
            target,
            ..CreateDialog::default()
        }
    }
}

pub(super) enum Outcome {
    Cancel,
    Create { dir: PathBuf, agent: NewAgent },
}

/// What the name field says: why the name cannot be used, else what it means.
fn name_check(
    name: &str,
    target: Option<&Target>,
    catalog: &Catalog,
) -> Result<Option<String>, String> {
    agent_defs::check_name(name).map_err(|e| e.to_string())?;
    let Some(t) = target else {
        return Err("choose where it goes".to_owned());
    };
    if let Some(f) = existing(catalog, &t.dir, name) {
        return Err(format!(
            "{} already defines {name} here",
            f.path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    let declared = |scope: Scope| {
        catalog
            .dirs
            .iter()
            .filter(|d| d.scope == scope)
            .flat_map(|d| &d.agents)
            .any(|a| a.loaded_name() == Some(name))
    };
    Ok(if t.scope == Scope::Project && declared(Scope::User) {
        Some(format!("Replaces your user agent {name} in this project"))
    } else if declared(Scope::Bundled) {
        Some(format!("Replaces omp's built-in {name} agent"))
    } else {
        None
    })
}

pub(super) fn show(
    ctx: &Context,
    d: &mut CreateDialog,
    targets: &[Target],
    catalog: &Catalog,
) -> Option<Outcome> {
    let pal = Palette::of_ctx(ctx);
    let name = d.name.trim().to_owned();
    let check = name_check(&name, targets.get(d.target), catalog);
    let ok = check.is_ok() && !d.description.trim().is_empty() && !d.busy;
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "agent-create",
            title: "New agent",
            width: 560.0,
            confirm_label: "Create",
            confirm_enabled: ok,
            ctrl_enter_confirms: true,
            icon: Some(ICON_FILE_PLUS),
            subtitle: Some("A Markdown file omp reads as a task agent"),
            ..Dialog::default()
        },
        |ui| {
            field(ui, "Name");
            let resp = ui.add(
                TextEdit::singleline(&mut d.name)
                    .id(egui::Id::unique("agent-create-name"))
                    .hint_text("api-reviewer")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 7)),
            );
            if !d.focused {
                resp.request_focus();
                d.focused = true;
            }
            let (text, color) = match (&check, name.is_empty()) {
                (_, true) => (
                    "Lowercase letters, digits and hyphens: also the file name".to_owned(),
                    pal.weak,
                ),
                (Err(e), false) => (e.clone(), pal.danger),
                (Ok(Some(note)), false) => (note.clone(), pal.accent_b),
                (Ok(None), false) => match targets.get(d.target) {
                    Some(t) => (abbreviate_home(&t.dir.join(format!("{name}.md"))), pal.weak),
                    None => (String::new(), pal.weak),
                },
            };
            ui.label(RichText::new(text).small().color(color));
            ui.add_space(8.0);

            field(ui, "Location");
            let current = targets.get(d.target).map(Target::label).unwrap_or_default();
            egui::ComboBox::from_id_salt("agent-create-target")
                .selected_text(current)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for (i, t) in targets.iter().enumerate() {
                        ui.selectable_value(&mut d.target, i, t.label())
                            .on_hover_text(t.dir.display().to_string());
                    }
                });
            let hint = match targets.get(d.target).map(|t| t.scope) {
                Some(Scope::User) => "Every session can use it",
                Some(_) => "Sessions in this project can use it",
                None => "",
            };
            ui.label(RichText::new(hint).small().color(pal.weak));
            ui.add_space(8.0);

            field(ui, "Description");
            ui.add(
                TextEdit::multiline(&mut d.description)
                    .id(egui::Id::unique("agent-create-description"))
                    .hint_text("What it does and when to use it: the parent model reads this to pick the agent")
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 7)),
            );
            ui.add_space(8.0);

            field(ui, "System prompt");
            egui::ScrollArea::vertical()
                .id_salt("agent-create-body")
                .max_height(220.0)
                .show(ui, |ui| {
                    ui.add(
                        TextEdit::multiline(&mut d.body)
                            .id(egui::Id::unique("agent-create-body-edit"))
                            .hint_text("Optional now: you can write it after creating the agent")
                            .code_editor()
                            .desired_rows(8)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 7)),
                    );
                });
            ui.label(
                RichText::new("Pick its model once it is created")
                    .small()
                    .color(pal.weak),
            );
            if let Some(e) = &d.error {
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(RichText::new(e).small().color(pal.danger))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            }
        },
    );
    match answer {
        Some(true) if ok => {
            let t = targets.get(d.target)?;
            d.busy = true;
            d.error = None;
            Some(Outcome::Create {
                dir: t.dir.clone(),
                agent: NewAgent {
                    name,
                    description: d.description.trim().to_owned(),
                    model: None,
                    body: d.body.trim().to_owned(),
                },
            })
        }
        Some(false) if !d.busy => Some(Outcome::Cancel),
        _ => None,
    }
}

fn field(ui: &mut Ui, label: &str) {
    let pal = Palette::of_ui(ui);
    ui.label(RichText::new(label).strong().color(pal.text));
    ui.add_space(2.0);
}

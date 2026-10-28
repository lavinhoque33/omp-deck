//! Settings → Agents: runs the page's actions (see `crate::agent_config`) over the gateway.

use deck_core::agent_defs::{self, AgentFile};
use deck_service::gateway::protocol::{Answer, Done, Mutation, Query};

use super::App;
use crate::agent_config::{AgentsAction, Part};

/// The file a create / customize / edit reply carries.
fn agent_file(result: Result<Done, String>) -> Result<AgentFile, String> {
    result.and_then(|d| match d {
        Done::AgentFile(f) => Ok(*f),
        other => Err(format!("unexpected reply {other:?}")),
    })
}

impl App {
    pub(super) fn apply_agents_action(&mut self, action: AgentsAction) {
        match action {
            AgentsAction::Load => self.load_agents(),
            AgentsAction::Edit {
                path,
                revision,
                edit,
            } => {
                let part = Part::of(&edit);
                let done_path = path.clone();
                self.gw.mutate(
                    Mutation::AgentEdit {
                        path,
                        revision,
                        edit,
                    },
                    move |app, result| {
                        let file = agent_file(result);
                        match &file {
                            Ok(f) => {
                                let what = match part {
                                    Part::Model => "model",
                                    Part::Body => "system prompt",
                                };
                                app.toasts.success(
                                    &app.ctx,
                                    format!("Saved {}'s {what}", f.display_name()),
                                );
                            }
                            Err(e) if part == Part::Body && agent_defs::is_changed_error(e) => {
                                app.toasts.info(
                                    &app.ctx,
                                    "The file changed on disk: choose which prompt to keep",
                                );
                            }
                            Err(e) => app.toasts.error(
                                &app.ctx,
                                format!("Agent not saved: {}", crate::summary::strip_context(e)),
                            ),
                        }
                        app.settings.agents.edit_done(&done_path, part, file);
                    },
                );
            }
            AgentsAction::SetSetting { key, value, done } => {
                self.gw.mutate(
                    Mutation::OmpSet {
                        key: key.to_owned(),
                        value: value.clone(),
                    },
                    move |app, result| {
                        match result {
                            Ok(_) => {
                                app.settings.write_done(key, Ok(Some(value)));
                                app.toasts.success(&app.ctx, done);
                            }
                            Err(e) => {
                                app.settings.write_done(key, Err(e.clone()));
                                app.toasts.error(
                                    &app.ctx,
                                    format!(
                                        "omp's config not changed: {}",
                                        crate::summary::strip_context(&e)
                                    ),
                                );
                            }
                        }
                        app.settings.agents.setting_done();
                        app.ctx.request_repaint();
                    },
                );
            }
            AgentsAction::Create { dir, agent } => {
                self.gw
                    .mutate(Mutation::AgentCreate { dir, agent }, |app, result| {
                        app.agent_written(agent_file(result), "Created");
                    });
            }
            AgentsAction::Customize { dir, name } => {
                self.gw
                    .mutate(Mutation::AgentCustomize { dir, name }, |app, result| {
                        app.agent_written(agent_file(result), "Customized");
                    });
            }
            AgentsAction::Delete {
                path,
                revision,
                name,
            } => {
                let done_path = path.clone();
                self.gw.mutate(
                    Mutation::AgentDelete { path, revision },
                    move |app, result| {
                        let result = result.map(|_| ());
                        match &result {
                            Ok(()) => app.toasts.success(&app.ctx, format!("Deleted {name}")),
                            Err(e) => app.toasts.error(
                                &app.ctx,
                                format!("{name} not deleted: {}", crate::summary::strip_context(e)),
                            ),
                        }
                        app.settings.agents.deleted(&done_path, result);
                        app.ctx.request_repaint();
                    },
                );
            }
            AgentsAction::ReloadModels => self.reload_models(),
            AgentsAction::ToggleFavorite(selector) => {
                self.cfg.toggle_favorite(&selector);
                self.dirty = true;
            }
            AgentsAction::ModelProvider(provider) => {
                self.cfg.model_provider_filter = provider;
                self.dirty = true;
            }
            AgentsAction::OpenFile(path) => self.open_path(path),
            AgentsAction::OpenFolder(dir) => self.open_folder(dir),
            AgentsAction::CopyPath(path) => self.copy_text(&path, "path"),
        }
    }

    /// A new definition file was written (or refused): `verb` names how, for the notice.
    fn agent_written(&mut self, file: Result<AgentFile, String>, verb: &str) {
        match &file {
            Ok(f) => self.toasts.success(
                &self.ctx,
                format!(
                    "{verb} {} in {}",
                    f.display_name(),
                    deck_core::sessions::abbreviate_home(f.path.parent().unwrap_or(&f.path))
                ),
            ),
            Err(e) => self.toasts.error(
                &self.ctx,
                format!("Agent not written: {}", crate::summary::strip_context(e)),
            ),
        }
        self.settings.agents.created(file);
        self.ctx.request_repaint();
    }

    /// Reread the agent definitions: the user's, those of every project with sessions, and
    /// omp's built-ins.
    fn load_agents(&mut self) {
        let projects = self
            .groups()
            .map(|g| g.iter().map(|g| g.cwd.clone()).collect())
            .unwrap_or_default();
        self.gw.query(Query::AgentDefs { projects }, |app, result| {
            let catalog = result.and_then(|a| match a {
                Answer::AgentDefs(c) => Ok(c),
                other => Err(format!("unexpected answer {other:?}")),
            });
            app.settings.agents.loaded(catalog);
            app.ctx.request_repaint();
        });
    }
}

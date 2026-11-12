//! App-side glue for the provider login wizard and Usage → Accounts: the `Topic::OmpAccounts`
//! subscription (held only while one of them shows), the wizard's requests, logout, and the
//! model catalog re-read after omp's credentials change.

use deck_service::gateway::protocol::{Done, Mutation, Topic};

use super::{App, PendingConfirm};
use crate::confirm::Confirm;
use crate::login_view::{self, LoginAction, LoginUi};
use crate::usage_view;

/// The wizard and what the window knows of omp's accounts.
#[derive(Default)]
pub(super) struct Accounts {
    /// The login wizard while it is open.
    pub(super) wizard: Option<LoginUi>,
    subscribed: bool,
    /// `OmpAccountsView::generation` last seen (kept while unsubscribed).
    generation: Option<u64>,
    /// The catalog must be read again once the load in flight finishes.
    models_stale: bool,
    /// Counts the wizard's login starts; closing or replacing the wizard moves it on too, so
    /// a start answered later knows whether the wizard that asked still waits for it.
    start_gen: u64,
}

impl App {
    /// Opens the login wizard, preselecting a login id or store id (resolved once omp's list
    /// arrives). An open wizard with a login running keeps it.
    pub(super) fn open_login(&mut self, preselect: Option<String>) {
        let view = self.gw.accounts();
        match &mut self.accounts.wizard {
            Some(w) if w.starting.is_some() || w.running_job(view.as_deref()).is_some() => {}
            Some(w) => {
                w.forget_fields(&self.ctx);
                *w = LoginUi::new(preselect);
                self.accounts.start_gen += 1;
            }
            None => {
                self.accounts.wizard = Some(LoginUi::new(preselect));
                self.accounts.start_gen += 1;
            }
        }
        self.drive_login();
    }

    /// Usage → Accounts.
    pub(super) fn open_accounts(&mut self) {
        self.usage_ui.page = usage_view::Page::Accounts;
        if !self.usage_open {
            self.open_usage();
        }
        self.drive_login();
    }

    /// Per frame: hold the accounts subscription while the wizard or Usage → Accounts shows,
    /// start a preselected login, open its browser once, and re-read the model catalog after a
    /// login or logout.
    pub(super) fn drive_login(&mut self) {
        let want = self.accounts.wizard.is_some()
            || (self.usage_open && self.usage_ui.page == usage_view::Page::Accounts);
        if want != self.accounts.subscribed {
            self.accounts.subscribed = want;
            if want {
                self.gw.subscribe(Topic::OmpAccounts);
            } else {
                self.gw.unsubscribe(&Topic::OmpAccounts);
            }
        }
        if self.accounts.models_stale && !self.models.is_loading() {
            self.accounts.models_stale = false;
            self.reload_models();
        }
        if !want {
            return;
        }
        let Some(view) = self.gw.accounts() else {
            return;
        };
        if self.accounts.generation != Some(view.generation) {
            // Not on the first sight: the catalog was read at startup.
            if self.accounts.generation.is_some() {
                if self.models.is_loading() {
                    self.accounts.models_stale = true;
                } else {
                    self.reload_models();
                }
            }
            self.accounts.generation = Some(view.generation);
        }
        let Some(w) = &mut self.accounts.wizard else {
            return;
        };
        if let Some(id) = w.take_preselect(&view) {
            self.start_login(id);
            return;
        }
        if let Some((job, url)) = login_view::auto_open(w, Some(&view)) {
            w.browser_opened = Some(job);
            self.open_external(url.to_owned());
        }
    }

    /// Per frame: draw the wizard (before the Usage view, so its Escape wins) and run what it
    /// asks for.
    pub(super) fn show_login(&mut self, ctx: &egui::Context) {
        let omp_found = self.omp_found();
        let view = self.gw.accounts().filter(|_| self.accounts.subscribed);
        let Some(w) = &mut self.accounts.wizard else {
            return;
        };
        let actions = login_view::show(ctx, w, view.as_deref(), omp_found);
        for a in actions {
            match a {
                LoginAction::Start(id) => self.start_login(id),
                LoginAction::Answer { job, seq, value } => self.gw.login_answer(job, seq, value),
                LoginAction::Cancel(job) => self.cancel_login(job),
                LoginAction::OpenUrl(url) => self.open_external(url),
                LoginAction::Copy(text, what) => self.copy_text(&text, what),
                LoginAction::Reload => {
                    self.gw.mutate(Mutation::OmpAccountsRefresh, |app, r| {
                        if let Err(e) = r {
                            app.toasts
                                .error(&app.ctx, format!("Could not read omp's providers: {e}"));
                        }
                    });
                }
                LoginAction::Close => {
                    // The known job is cancelled even before the view shows it; a stale cancel
                    // is harmless.
                    let job = self.accounts.wizard.as_mut().and_then(|w| {
                        w.forget_fields(ctx);
                        w.job_to_cancel(view.as_deref())
                    });
                    if let Some(job) = job {
                        self.cancel_login(job);
                    }
                    self.accounts.wizard = None;
                    self.accounts.start_gen += 1;
                    self.focus_pending = true;
                    self.drive_login();
                    return;
                }
            }
        }
    }

    /// Asks the gateway to start omp's login to `id`; the wizard follows the job it returns.
    fn start_login(&mut self, id: String) {
        let Some(w) = &mut self.accounts.wizard else {
            return;
        };
        let name = self
            .gw
            .accounts()
            .and_then(|v| {
                v.providers
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_else(|| id.clone());
        w.begin(id.clone());
        self.accounts.start_gen += 1;
        let start = self.accounts.start_gen;
        self.gw
            .mutate(Mutation::OmpLogin { provider: id }, move |app, r| {
                let current = app.accounts.start_gen == start;
                let Some(w) = app.accounts.wizard.as_mut().filter(|_| current) else {
                    // The wizard that asked closed (another may have opened since): the login
                    // must not run unseen, nor be adopted by a wizard that did not start it.
                    if let Ok(Done::OmpLogin { job }) = r {
                        app.cancel_login(job);
                    }
                    return;
                };
                match r {
                    Ok(Done::OmpLogin { job }) => w.started(job),
                    Ok(other) => w.refuse(name, format!("unexpected answer {other:?}")),
                    Err(e) => w.refuse(name, e),
                }
                app.ctx.request_repaint();
            });
    }

    fn cancel_login(&mut self, job: u64) {
        self.gw.mutate(Mutation::OmpLoginCancel { job }, |app, r| {
            if let Err(e) = r {
                app.toasts
                    .error(&app.ctx, format!("Could not cancel the login: {e}"));
            }
        });
    }

    /// Usage → Accounts asked to log out a stored credential: confirm first.
    pub(super) fn ask_logout(
        &mut self,
        provider: String,
        credential_id: i64,
        label: String,
        name: String,
    ) {
        self.confirm = Some((
            Confirm {
                title: "Log out?".to_owned(),
                body: format!(
                    "omp removes the stored credential {label} of {name}. Running sessions stop \
                     using it on their next request. Keys omp reads from the environment or its \
                     config stay."
                ),
                confirm_label: "Log out".to_owned(),
                danger: true,
            },
            PendingConfirm::Logout {
                provider,
                credential_id,
            },
        ));
    }

    pub(super) fn execute_logout(&mut self, provider: String, credential_id: i64) {
        self.gw.mutate(
            Mutation::OmpLogout {
                provider,
                credential_id,
            },
            |app, r| match r {
                Ok(Done::OmpLogout(out)) if out.removed => match out.remaining {
                    Some(note) => app.toasts.success(&app.ctx, format!("Logged out. {note}")),
                    None => app.toasts.success(&app.ctx, "Logged out"),
                },
                Ok(Done::OmpLogout(_)) => app
                    .toasts
                    .info(&app.ctx, "omp had no such credential stored any more"),
                Ok(other) => app
                    .toasts
                    .error(&app.ctx, format!("Log out: unexpected answer {other:?}")),
                Err(e) => app
                    .toasts
                    .error(&app.ctx, format!("Could not log out: {e}")),
            },
        );
    }
}

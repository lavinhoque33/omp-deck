//! Native provider authentication: one transient login worker, independent of job lanes.
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use super::{Engine, Fin, Msg, Sched};
use crate::gateway::protocol::{
    GatewayError, LoginAuth, LoginOutcome, LoginPrompt, LoginView, OmpAccountsView, SecretText,
    Topic,
};
use deck_core::omp::auth::{LoginEvent, LoginProvider, LoginRun};

const POLL: Duration = Duration::from_millis(100);
const LIST_AGE: Duration = Duration::from_secs(60);
const RESULT_AGE: Duration = Duration::from_secs(600);

#[derive(Default)]
pub(super) struct Auth {
    pub(super) view: OmpAccountsView,
    worker: Option<Sender<Input>>,
    loaded: Option<Instant>,
    reload: bool,
    finished: Option<Instant>,
}

enum Input {
    Answer(u64, SecretText),
    Cancel,
}
pub(crate) enum AuthFin {
    List(Result<Vec<LoginProvider>, String>),
    Events { job: u64, events: Vec<LoginEvent> },
}

impl Auth {
    pub(super) fn running(&self) -> bool {
        self.worker.is_some()
    }
    pub(super) fn login_refusal(&self, installed: bool) -> Option<GatewayError> {
        if self.running() {
            let name = self.view.login.as_ref().map_or("", |v| v.name.as_str());
            Some(GatewayError::Refused(format!(
                "Another login is in progress ({name})"
            )))
        } else if !installed {
            Some(GatewayError::Refused("omp was not found".into()))
        } else {
            None
        }
    }
    pub(super) fn store(&self, provider: &str) -> String {
        self.view
            .providers
            .iter()
            .find(|p| p.id == provider)
            .map_or_else(|| provider.to_owned(), |p| p.store.clone())
    }
    fn fold(&mut self, job: u64, events: Vec<LoginEvent>) -> Fold {
        let mut folded = Fold::default();
        let Some(view) = self
            .view
            .login
            .as_mut()
            .filter(|v| v.job == job && v.outcome.is_none())
        else {
            return folded;
        };
        for event in events {
            if view.outcome.is_some() {
                break;
            }
            folded.changed = true;
            match event {
                LoginEvent::Auth {
                    url,
                    launch_url,
                    instructions,
                } => {
                    view.auth = Some(LoginAuth {
                        url,
                        launch_url,
                        instructions,
                    })
                }
                LoginEvent::Prompt {
                    seq,
                    message,
                    placeholder,
                    allow_empty,
                    masked,
                } => {
                    view.prompt = Some(LoginPrompt {
                        seq,
                        message,
                        placeholder,
                        allow_empty,
                        masked,
                    })
                }
                LoginEvent::Manual { seq } => view.manual = Some(seq),
                LoginEvent::Progress(message) => {
                    if view.progress.len() == 6 {
                        view.progress.remove(0);
                    }
                    view.progress.push(message);
                }
                LoginEvent::Done { store, label } => {
                    view.outcome = Some(LoginOutcome::Done { label });
                    self.view.generation += 1;
                    folded.store = Some(store);
                }
                LoginEvent::Failed { message, cancelled } => {
                    view.outcome = Some(if cancelled {
                        LoginOutcome::Cancelled
                    } else {
                        LoginOutcome::Failed(message)
                    })
                }
            }
        }
        if view.outcome.is_some() {
            view.prompt = None;
            view.manual = None;
            self.worker = None;
            self.finished = Some(Instant::now());
            folded.finished = true;
        }
        folded
    }
}
#[derive(Default)]
struct Fold {
    changed: bool,
    finished: bool,
    store: Option<String>,
}

// The worker accepts an event source rather than assuming a real omp process, so the
// state machine and command delivery can be exercised deterministically.
trait EventSource {
    fn poll(&mut self) -> Vec<LoginEvent>;
    fn answer(&self, seq: u64, value: &str) -> io::Result<()>;
    fn cancel(&mut self);
}
impl EventSource for LoginRun {
    fn poll(&mut self) -> Vec<LoginEvent> {
        LoginRun::poll(self)
    }
    fn answer(&self, seq: u64, value: &str) -> io::Result<()> {
        LoginRun::answer(self, seq, value)
    }
    fn cancel(&mut self) {
        LoginRun::cancel(self);
    }
}
fn drive(
    mut source: impl EventSource,
    input: Receiver<Input>,
    mut emit: impl FnMut(Vec<LoginEvent>) -> bool,
) {
    loop {
        let events = source.poll();
        let done = events
            .iter()
            .any(|e| matches!(e, LoginEvent::Done { .. } | LoginEvent::Failed { .. }));
        if !events.is_empty() && !emit(events) {
            return;
        }
        if done {
            return;
        }
        match input.recv_timeout(POLL) {
            Ok(Input::Answer(seq, value)) => {
                if let Err(error) = source.answer(seq, value.expose()) {
                    emit(vec![LoginEvent::Failed {
                        message: format!("Cannot deliver login answer: {error}"),
                        cancelled: false,
                    }]);
                    return;
                }
            }
            Ok(Input::Cancel) => source.cancel(),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl Engine {
    pub(super) fn ensure_omp_accounts(&mut self) {
        if self.auth.loaded.is_none_or(|at| at.elapsed() >= LIST_AGE) && !self.auth.view.loading {
            self.load_omp_accounts();
        }
    }
    pub(super) fn load_omp_accounts(&mut self) {
        if self.auth.view.loading {
            self.auth.reload = true;
            return;
        }
        let Some(omp) = self.omp.clone() else {
            self.auth.view.error = Some("omp was not found".into());
            self.changed(Topic::OmpAccounts);
            return;
        };
        self.auth.view.loading = true;
        self.auth.view.error = None;
        self.changed(Topic::OmpAccounts);
        self.spawn_worker("omp-accounts", move || {
            Fin::Auth(AuthFin::List(
                omp.login_providers().map_err(|e| e.to_string()),
            ))
        });
    }
    pub(super) fn start_omp_login(&mut self, provider: String) -> Result<u64, GatewayError> {
        if let Some(error) = self.auth.login_refusal(self.omp.is_some()) {
            return Err(error);
        }
        let omp = self
            .omp
            .clone()
            .ok_or_else(|| GatewayError::Refused("omp was not found".into()))?;
        // Jobs cross reconnects without an epoch field. An OS-random identity prevents a
        // wizard from a previous owner from addressing a new owner's login.
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes)
            .map_err(|e| GatewayError::Failed(format!("Cannot identify login job: {e}")))?;
        let job = u64::from_le_bytes(bytes);
        let name = self
            .auth
            .view
            .providers
            .iter()
            .find(|p| p.id == provider)
            .map_or_else(|| provider.clone(), |p| p.name.clone());
        let (tx, rx) = mpsc::channel();
        let out = self.tx.clone();
        let login_provider = provider.clone();
        std::thread::Builder::new()
            .name("gateway-omp-login".into())
            .spawn(move || {
                let emit = |events| {
                    out.send(Msg::Fin(Box::new(Fin::Auth(AuthFin::Events {
                        job,
                        events,
                    }))))
                    .is_ok()
                };
                match omp.login(&login_provider) {
                    Ok(run) => drive(run, rx, emit),
                    Err(error) => {
                        emit(vec![LoginEvent::Failed {
                            message: error.to_string(),
                            cancelled: false,
                        }]);
                    }
                }
            })
            .map_err(|e| GatewayError::Failed(e.to_string()))?;
        self.auth.worker = Some(tx);
        self.auth.finished = None;
        self.auth.view.login = Some(LoginView {
            job,
            provider,
            name,
            auth: None,
            prompt: None,
            manual: None,
            progress: Vec::new(),
            outcome: None,
        });
        self.changed(Topic::OmpAccounts);
        Ok(job)
    }
    pub(super) fn cancel_omp_login(&self, job: u64) {
        if self.auth.view.login.as_ref().is_some_and(|v| v.job == job)
            && let Some(tx) = &self.auth.worker
        {
            let _ = tx.send(Input::Cancel);
        }
    }
    pub(super) fn login_answer(&mut self, job: u64, seq: u64, value: SecretText) {
        let Some(view) = self
            .auth
            .view
            .login
            .as_mut()
            .filter(|v| v.job == job && v.outcome.is_none())
        else {
            return;
        };
        let prompt = view.prompt.as_ref().is_some_and(|p| p.seq == seq);
        let manual = view.manual == Some(seq);
        if !(prompt || manual) {
            return;
        }
        if let Some(tx) = &self.auth.worker {
            let _ = tx.send(Input::Answer(seq, value));
            if prompt {
                view.prompt = None;
            }
            if manual {
                view.manual = None;
            }
            self.changed(Topic::OmpAccounts);
        }
    }
    pub(super) fn step_auth(&mut self, now: Instant) {
        if self
            .auth
            .finished
            .is_some_and(|at| now.saturating_duration_since(at) >= RESULT_AGE)
        {
            self.auth.finished = None;
            self.auth.view.login = None;
            self.changed(Topic::OmpAccounts);
        }
    }
    pub(super) fn on_auth_fin(&mut self, fin: AuthFin) {
        match fin {
            AuthFin::List(result) => {
                self.auth.view.loading = false;
                self.auth.loaded = Some(Instant::now());
                match result {
                    Ok(providers) => {
                        self.auth.view.providers = providers;
                        self.auth.view.error = None;
                    }
                    Err(error) => self.auth.view.error = Some(error),
                }
                self.changed(Topic::OmpAccounts);
                if std::mem::take(&mut self.auth.reload) {
                    self.load_omp_accounts();
                }
            }
            AuthFin::Events { job, events } => {
                let folded = self.auth.fold(job, events);
                if folded.changed {
                    self.changed(Topic::OmpAccounts);
                }
                if folded.finished {
                    self.load_omp_accounts();
                }
                if let Some(store) = folded.store {
                    self.refresh_after_auth(&store);
                }
            }
        }
    }
    pub(super) fn omp_auth_changed(&mut self, store: &str) {
        self.auth.view.generation += 1;
        self.changed(Topic::OmpAccounts);
        self.refresh_after_auth(store);
    }
    fn refresh_after_auth(&mut self, store: &str) {
        if !self.sched.usage_fetching() {
            self.sched.begin_usage_fetch(Instant::now());
            self.dirty.usage = true;
            let ctx = self.job_ctx();
            self.spawn_worker("usage", move || {
                Sched::Usage(super::mutate::fetch_usage(&ctx)).into()
            });
        }
        self.refresh_live_provider(store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Script(VecDeque<Vec<LoginEvent>>);
    impl EventSource for Script {
        fn poll(&mut self) -> Vec<LoginEvent> {
            self.0.pop_front().expect("script has terminal event")
        }
        fn answer(&self, _: u64, _: &str) -> io::Result<()> {
            panic!("no answer expected")
        }
        fn cancel(&mut self) {
            panic!("no cancellation expected")
        }
    }
    fn active() -> Auth {
        let (tx, _rx) = mpsc::channel();
        Auth {
            view: OmpAccountsView {
                login: Some(LoginView {
                    job: 7,
                    provider: "p".into(),
                    name: "Provider".into(),
                    auth: None,
                    prompt: None,
                    manual: None,
                    progress: vec![],
                    outcome: None,
                }),
                ..OmpAccountsView::default()
            },
            worker: Some(tx),
            ..Auth::default()
        }
    }
    #[test]
    fn second_login_refuses_the_gateway_wide_slot() {
        let auth = active();
        assert!(
            matches!(auth.login_refusal(true), Some(GatewayError::Refused(message)) if message == "Another login is in progress (Provider)")
        );
        assert!(
            matches!(Auth::default().login_refusal(false), Some(GatewayError::Refused(message)) if message == "omp was not found")
        );
    }
    #[test]
    fn scripted_login_folds_each_step_and_success_once() {
        let mut auth = active();
        let script = Script(VecDeque::from([
            vec![LoginEvent::Auth {
                url: "https://provider/".into(),
                launch_url: Some("http://localhost/launch".into()),
                instructions: Some("Sign in".into()),
            }],
            vec![LoginEvent::Prompt {
                seq: 1,
                message: "API key".into(),
                placeholder: Some("sk-".into()),
                allow_empty: false,
                masked: true,
            }],
            (0..8)
                .map(|n| LoginEvent::Progress(format!("progress {n}")))
                .collect(),
            vec![LoginEvent::Done {
                store: "store".into(),
                label: Some("user".into()),
            }],
        ]));
        let (_tx, rx) = mpsc::channel();
        let mut step = 0;
        drive(script, rx, |events| {
            let folded = auth.fold(7, events);
            assert!(folded.changed);
            let view = auth.view.login.as_ref().unwrap();
            match step {
                0 => assert_eq!(
                    view.auth,
                    Some(LoginAuth {
                        url: "https://provider/".into(),
                        launch_url: Some("http://localhost/launch".into()),
                        instructions: Some("Sign in".into())
                    })
                ),
                1 => assert_eq!(
                    view.prompt,
                    Some(LoginPrompt {
                        seq: 1,
                        message: "API key".into(),
                        placeholder: Some("sk-".into()),
                        allow_empty: false,
                        masked: true
                    })
                ),
                2 => assert_eq!(
                    view.progress,
                    (2..8).map(|n| format!("progress {n}")).collect::<Vec<_>>()
                ),
                3 => {
                    assert_eq!(
                        view.outcome,
                        Some(LoginOutcome::Done {
                            label: Some("user".into())
                        })
                    );
                    assert!(view.prompt.is_none());
                    assert_eq!(folded.store.as_deref(), Some("store"));
                    assert!(!auth.running());
                }
                _ => panic!("unexpected event"),
            }
            assert_eq!(auth.view.generation, u64::from(step == 3));
            step += 1;
            true
        });
        assert_eq!(step, 4);
        assert!(
            !auth
                .fold(
                    7,
                    vec![LoginEvent::Done {
                        store: "store".into(),
                        label: None
                    }]
                )
                .changed
        );
        assert_eq!(auth.view.generation, 1);
    }
    #[test]
    fn failure_cancellation_and_wrong_job_never_increment_generation() {
        for cancelled in [false, true] {
            let mut auth = active();
            assert!(
                !auth
                    .fold(
                        8,
                        vec![LoginEvent::Done {
                            store: "store".into(),
                            label: None
                        }]
                    )
                    .changed
            );
            let result = auth.fold(
                7,
                vec![LoginEvent::Failed {
                    message: "denied".into(),
                    cancelled,
                }],
            );
            assert!(result.finished);
            assert!(result.store.is_none());
            assert_eq!(auth.view.generation, 0);
            assert_eq!(
                auth.view.login.unwrap().outcome,
                Some(if cancelled {
                    LoginOutcome::Cancelled
                } else {
                    LoginOutcome::Failed("denied".into())
                })
            );
        }
    }
}

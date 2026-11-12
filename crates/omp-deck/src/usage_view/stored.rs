//! Usage → Accounts: the credentials omp has stored (from `Topic::OmpAccounts`), one card per
//! provider, with the usage state merged in and the login wizard's entry points: "Add account…",
//! "Log in again" on the accounts omp cannot use, "Log out…".

use deck_core::omp::auth::{LoginProvider, StoredAccount};
use deck_service::gateway::protocol::OmpAccountsView;
use egui::{Align, Align2, CursorIcon, Id, Layout, RichText, Sense, Stroke, StrokeKind, Ui, pos2};

use super::{UsageAction, card_frame};
use crate::badge::{self, Hue};
use crate::icons::{ICON_KEY_ROUND, ICON_LOG_IN, ICON_LOG_OUT, ICON_PLUS, ICON_USER};
use crate::theme::{self, Palette};
use crate::usage_state::CredentialIssue;
use crate::widgets::{DraftButton, DraftKind, Spin, icon_button, icon_text, menu_item};

const ROW_H: f32 = 42.0;

/// One credential store (aliases such as `devin-cli` / `devin-windsurf` share one): the login
/// id omp's logout takes, the name shown, the env/config origin and the stored accounts.
#[derive(Debug, PartialEq)]
pub struct StoreGroup<'a> {
    pub store: &'a str,
    pub login_id: &'a str,
    pub name: &'a str,
    /// An environment variable or config entry omp also reads (never logged out here).
    pub external: Option<String>,
    pub accounts: &'a [StoredAccount],
}

/// The stores signed in, in omp's order: stored accounts or an env/config key.
pub fn store_groups(providers: &[LoginProvider]) -> Vec<StoreGroup<'_>> {
    let mut out: Vec<StoreGroup<'_>> = Vec::new();
    for p in providers {
        if out.iter().any(|g| g.store == p.store) {
            continue;
        }
        // The alias listing the stored accounts is the one omp's logout takes; the entry that
        // owns the store names the group.
        let same = || providers.iter().filter(|q| q.store == p.store);
        let owner = same().find(|q| !q.accounts.is_empty());
        let main = same().find(|q| q.id == p.store).or(owner).unwrap_or(p);
        let external = same().find_map(|q| q.origin.as_ref()).and_then(|o| {
            match (o.kind.as_str(), &o.env_var) {
                ("env", Some(v)) => Some(format!("env: {v}")),
                ("env", None) => Some("environment".to_owned()),
                // A stored credential is listed as an account already.
                ("oauth" | "api_key", _) => None,
                (other, _) => Some(other.to_owned()),
            }
        });
        if owner.is_none() && external.is_none() {
            continue;
        }
        out.push(StoreGroup {
            store: &p.store,
            login_id: &owner.unwrap_or(main).id,
            name: &main.name,
            external,
            accounts: owner.map_or(&[][..], |q| q.accounts.as_slice()),
        });
    }
    out
}

/// The stored-accounts section of the Accounts page.
pub fn section(
    ui: &mut Ui,
    view: Option<&OmpAccountsView>,
    issues: &[CredentialIssue],
    out: &mut Vec<UsageAction>,
) {
    let pal = Palette::of_ui(ui);
    egui::Sides::new().height(36.0).show(
        ui,
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(theme::strong("Signed in").size(15.0).color(pal.text_strong));
                ui.label(
                    RichText::new("Accounts omp has stored. Add or remove them here; omp keeps the credentials.")
                        .small()
                        .color(pal.weak),
                );
            });
        },
        |ui| {
            if DraftButton::new("Add account…")
                .kind(DraftKind::Primary)
                .icon(ICON_PLUS)
                .show(ui)
                .on_hover_text("Log in to a provider (omp stores the credential)")
                .clicked()
            {
                out.push(UsageAction::Login(None));
            }
            if view.is_some_and(|v| v.loading) {
                ui.add(Spin::new(16.0, pal.accent));
            }
        },
    );
    ui.add_space(crate::widgets::DRAFT_BELOW);
    let Some(view) = view else {
        loading_line(ui);
        return;
    };
    if let Some(e) = &view.error {
        ui.label(
            RichText::new(format!("Could not read omp's accounts: {e}"))
                .small()
                .color(pal.danger),
        );
    }
    let groups = store_groups(&view.providers);
    if groups.is_empty() {
        if view.loading {
            loading_line(ui);
        } else if view.error.is_none() {
            ui.label(
                RichText::new("Nothing is signed in yet: add an account to start.").color(pal.weak),
            );
        }
        return;
    }
    for g in &groups {
        group_card(ui, g, issues, out);
    }
}

fn loading_line(ui: &mut Ui) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.add(Spin::new(14.0, pal.accent));
        ui.label(RichText::new("Reading omp's accounts…").color(pal.weak));
    });
}

fn group_card(
    ui: &mut Ui,
    g: &StoreGroup<'_>,
    issues: &[CredentialIssue],
    out: &mut Vec<UsageAction>,
) {
    let pal = Palette::of_ui(ui);
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::strong(g.name).size(14.5).color(pal.text_strong));
            ui.label(RichText::new(g.store).small().color(pal.weak));
            let n = g.accounts.len();
            if n > 1 {
                ui.label(
                    RichText::new(format!("{n} accounts"))
                        .small()
                        .color(pal.weak),
                );
            }
        });
        let lone_key = g.accounts.iter().filter(|a| a.kind == "api_key").count() == 1;
        for (i, a) in g.accounts.iter().enumerate() {
            let issue = issues
                .iter()
                .find(|x| x.matches(g.store, &a.label, &a.kind, lone_key));
            account_row(ui, g, a, issue, i, out);
        }
        if let Some(ext) = &g.external {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(icon_text(ICON_KEY_ROUND, 12.0, pal.weak));
                badge::show(ui, None, ext, Hue::Amber, 10.5);
                ui.label(
                    RichText::new(if ext.starts_with("env") {
                        "omp reads this key from the environment; unset it to sign out"
                    } else {
                        "omp reads this key from its config"
                    })
                    .small()
                    .color(pal.weak),
                );
            });
        }
    });
}

/// One stored account: label and detail, type and Active badges, the usage issue merged in;
/// "Log in again" / "Log out…" as hover buttons and in the context menu.
fn account_row(
    ui: &mut Ui,
    g: &StoreGroup<'_>,
    a: &StoredAccount,
    issue: Option<&CredentialIssue>,
    i: usize,
    out: &mut Vec<UsageAction>,
) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::hover());
    let resp = ui.interact(
        rect,
        Id::unique(("stored-account", g.store, a.credential_id, i)),
        Sense::click(),
    );
    let hovered = ui.rect_contains_pointer(rect);
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), hovered, 0.08);
    let p = ui.painter();
    p.rect(
        rect,
        theme::rad(6, 9),
        theme::hover_fill(pal).gamma_multiply(hov),
        Stroke::new(
            1.0,
            theme::mix(pal.separator, theme::hover_stroke(pal), hov),
        ),
        StrokeKind::Inside,
    );
    let icon = if a.kind == "oauth" {
        ICON_USER
    } else {
        ICON_KEY_ROUND
    };
    let x = rect.left() + 12.0;
    crate::widgets::paint_icon(
        p,
        pos2(x + 7.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        pal.glyph,
    );
    let tx = x + 24.0;
    p.text(
        pos2(tx, rect.top() + 6.0),
        Align2::LEFT_TOP,
        &a.label,
        theme::font(13.5),
        pal.text_strong,
    );
    let sub = match issue {
        Some(x) if a.detail.is_empty() => x.reason.clone(),
        Some(x) => format!("{} · {}", a.detail, x.reason),
        None => a.detail.clone(),
    };
    if !sub.is_empty() {
        p.text(
            pos2(tx, rect.bottom() - 6.0),
            Align2::LEFT_BOTTOM,
            &sub,
            theme::font(11.5),
            if issue.is_some() { pal.warn } else { pal.weak },
        );
    }
    let relog = issue.is_some();
    let mut right = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(10.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    right.spacing_mut().item_spacing.x = 6.0;
    if hovered {
        if icon_button(&mut right, ICON_LOG_OUT, "Log out…", false).clicked() {
            out.push(logout(g, a));
        }
        if relog && icon_button(&mut right, ICON_LOG_IN, "Log in again", false).clicked() {
            out.push(UsageAction::Login(Some(g.login_id.to_owned())));
        }
    }
    if a.active {
        badge::show(&mut right, None, "Active", Hue::Green, 10.5);
    }
    match issue {
        Some(x) if x.disabled => {
            badge::show(&mut right, None, "disabled", Hue::Rose, 10.5);
        }
        Some(_) => {
            badge::show(&mut right, None, "no usage", Hue::Amber, 10.5);
        }
        None => {}
    }
    let (kind, hue) = match a.kind.as_str() {
        "oauth" => ("OAuth", Hue::Blue),
        "api_key" => ("API key", Hue::Violet),
        other => (other, Hue::Teal),
    };
    badge::show(&mut right, None, kind, hue, 10.5);
    let resp = resp.on_hover_cursor(CursorIcon::ContextMenu);
    theme::context_menu(&resp, 180.0, |ui| {
        if relog && menu_item(ui, ICON_LOG_IN, "Log in again", None, true, false).clicked() {
            out.push(UsageAction::Login(Some(g.login_id.to_owned())));
            ui.close();
        }
        if menu_item(ui, ICON_LOG_OUT, "Log out…", None, true, true).clicked() {
            out.push(logout(g, a));
            ui.close();
        }
    });
}

fn logout(g: &StoreGroup<'_>, a: &StoredAccount) -> UsageAction {
    UsageAction::Logout {
        provider: g.login_id.to_owned(),
        credential_id: a.credential_id,
        label: a.label.clone(),
        name: g.name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use deck_core::omp::auth::AuthOrigin;

    use super::*;

    fn provider(id: &str, store: &str) -> LoginProvider {
        LoginProvider {
            id: id.into(),
            name: format!("{id} name"),
            available: true,
            store: store.into(),
            disabled: false,
            origin: None,
            accounts: Vec::new(),
        }
    }

    fn account(id: i64, label: &str) -> StoredAccount {
        StoredAccount {
            credential_id: id,
            label: label.into(),
            detail: String::new(),
            kind: "oauth".into(),
            active: id == 1,
        }
    }

    #[test]
    fn aliases_of_one_store_make_one_group_and_env_only_stores_cannot_log_out() {
        let mut devin_cli = provider("devin-cli", "devin");
        devin_cli.accounts.push(account(1, "a@x.test"));
        let mut devin_ws = provider("devin-windsurf", "devin");
        devin_ws.accounts.push(account(1, "a@x.test"));
        let mut openai = provider("openai", "openai");
        openai.origin = Some(AuthOrigin {
            kind: "env".into(),
            env_var: Some("OPENAI_API_KEY".into()),
        });
        let mut zai = provider("zai-coding-plan", "zai");
        zai.accounts.push(account(2, "key"));
        zai.origin = Some(AuthOrigin {
            kind: "api_key".into(),
            env_var: None,
        });
        let list = vec![
            provider("anthropic", "anthropic"),
            devin_cli,
            devin_ws,
            openai,
            zai,
        ];
        let groups = store_groups(&list);
        let summary: Vec<_> = groups
            .iter()
            .map(|g| (g.store, g.login_id, g.accounts.len(), g.external.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("devin", "devin-cli", 1, None),
                ("openai", "openai", 0, Some("env: OPENAI_API_KEY")),
                ("zai", "zai-coding-plan", 1, None),
            ]
        );
        assert_eq!(groups[0].name, "devin-cli name");
    }
}

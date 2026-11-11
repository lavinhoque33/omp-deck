//! The shared model preferences over the device protocol, through the real relay and Noise path:
//! `model_favorite` (an explicit desired state, idempotent, starring order kept, unavailable
//! entries kept) and `model_provider`, both written by the gateway's settings service so the
//! desktop's config file shows them, with unrelated settings untouched; `models` answers with the
//! current preferences; a view-only device cannot change them and an old (non-`s4`) device can.
//!
//! Private HOME/XDG tree, own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use deck_core::config::AppConfig;
use deck_remote::api::{ApiError, Class, DoneView, Op, OutcomeView, ReplyResult, Resp};
use deck_service::gateway::client::Link;
use deck_service::gateway::protocol::{Mutation, Permissions, RelayLink, SecretText};
use support::device::{
    Device, OMP_CONFIG, Relay, Sandbox, accepted, pair, run, wait_for, wait_mobile,
};
use support::fake_model::{FakeModel, Reply};

const STEP: Duration = Duration::from_secs(30);

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// Runs one journaled preference op to its final outcome.
fn mutate(d: &mut Device, seq: &mut u64, op: Op) {
    let n = *seq;
    *seq += 1;
    assert!(!accepted(d.call(&mut [], Some(n), op)));
    match d.outcome(&mut [], n, STEP) {
        OutcomeView::Done { result } => assert!(matches!(result, DoneView::Unit {}), "{result:?}"),
        other => panic!("not done: {other:?}"),
    }
}

/// What `models` answers: the catalog's selectors, the favorites and the provider filter.
fn models(d: &mut Device) -> (Vec<String>, Vec<String>, Option<String>) {
    match d.call(&mut [], None, Op::Models {}) {
        ReplyResult::Ok(Resp::Models {
            models,
            favorites,
            provider,
            ..
        }) => (
            models.into_iter().map(|m| m.selector).collect(),
            favorites,
            provider,
        ),
        other => panic!("models: {other:?}"),
    }
}

#[test]
fn model_preferences_roundtrip_through_the_shared_settings() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let sb = Sandbox::new("rdmp", OMP_CONFIG);
    sb.report_failures();
    let model = FakeModel::start(|_: &serde_json::Value| Reply::text("ok"));
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    // The desktop's shared config before the phone speaks: an unavailable favorite and
    // unrelated settings that must survive every phone write.
    std::fs::write(
        &sb.profile.settings,
        br#"{"gateway_mode":"Background","check_app_updates":false,"favorite_models":["gone/old"],"model_provider_filter":"vanished"}"#,
    )
    .expect("settings");
    let relay = Relay::start(sb.root.join("relay"));
    let (phone, viewer) = {
        let client = sb.client();
        wait_for("the gateway link", Duration::from_secs(30), || {
            matches!(client.link(), Link::Up(_)).then_some(())
        });
        run(
            &client,
            Mutation::RelaySet {
                url: relay.url(),
                room_key: SecretText::new(relay.key.encode()),
            },
        )
        .expect("relay set");
        wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
        (
            pair(
                &client,
                &relay,
                "Phone",
                Permissions {
                    control: true,
                    manage: false,
                },
            ),
            pair(
                &client,
                &relay,
                "Viewer",
                Permissions {
                    control: false,
                    manage: false,
                },
            ),
        )
    };

    // The phone does not announce `s4`: the new operations are not gated.
    let mut a = Device::connect(&relay, &phone, "Phone", false);
    let mut seq = a.welcome.next_seq;

    let (catalog, favorites, provider) = models(&mut a);
    assert!(catalog.contains(&"fake/fake-1".to_owned()), "{catalog:?}");
    assert_eq!(favorites, ["gone/old"]);
    assert_eq!(provider.as_deref(), Some("vanished"));

    let fav = |selector: &str, favorite: bool| Op::ModelFavorite {
        selector: selector.to_owned(),
        favorite,
    };
    mutate(&mut a, &mut seq, fav("fake/fake-1", true));
    // A repeat of the desired state changes nothing (no toggle).
    mutate(&mut a, &mut seq, fav("fake/fake-1", true));
    mutate(&mut a, &mut seq, fav("other/second", true));
    mutate(&mut a, &mut seq, fav("fake/fake-1", true));
    let (_, favorites, _) = models(&mut a);
    assert_eq!(favorites, ["gone/old", "fake/fake-1", "other/second"]);

    // Unstarring an unavailable entry keeps the order of the rest; a repeat is a no-op.
    mutate(&mut a, &mut seq, fav("gone/old", false));
    mutate(&mut a, &mut seq, fav("gone/old", false));
    let (_, favorites, _) = models(&mut a);
    assert_eq!(favorites, ["fake/fake-1", "other/second"]);

    mutate(
        &mut a,
        &mut seq,
        Op::ModelProvider {
            provider: Some("fake".into()),
        },
    );
    let (_, favorites, provider) = models(&mut a);
    assert_eq!(favorites, ["fake/fake-1", "other/second"]);
    assert_eq!(provider.as_deref(), Some("fake"));

    // The desktop reads the same file: the phone's state, nothing else touched.
    let cfg = AppConfig::load_from(&sb.profile.settings);
    assert_eq!(cfg.favorite_models, ["fake/fake-1", "other/second"]);
    assert_eq!(cfg.model_provider_filter.as_deref(), Some("fake"));
    assert!(!cfg.check_app_updates, "an unrelated setting was lost");

    // A view-only device cannot change them.
    let mut v = Device::connect(&relay, &viewer, "Viewer", true);
    let vseq = v.welcome.next_seq;
    for op in [
        fav("fake/fake-1", false),
        Op::ModelProvider { provider: None },
    ] {
        let r = v.call(&mut [], Some(vseq), op);
        assert!(
            matches!(
                r,
                ReplyResult::Err(ApiError::NotAllowed {
                    need: Class::Control
                })
            ),
            "{r:?}"
        );
    }
    let cfg = AppConfig::load_from(&sb.profile.settings);
    assert_eq!(cfg.favorite_models, ["fake/fake-1", "other/second"]);
    assert_eq!(cfg.model_provider_filter.as_deref(), Some("fake"));

    // Back to All providers (persisted as none, shown to the viewer's reads as none).
    mutate(&mut a, &mut seq, Op::ModelProvider { provider: None });
    let (_, _, provider) = models(&mut v);
    assert_eq!(provider, None);
    assert_eq!(
        AppConfig::load_from(&sb.profile.settings).model_provider_filter,
        None
    );
}

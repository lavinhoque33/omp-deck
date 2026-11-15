//! A controller generation is not the identity of the extension's pending tool connection.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use deck_core::config::SessionView;
use deck_service::gateway::client::{ClientEvent, GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Answer, Done, LaunchSpec, Mutation, Query, Role, SessionRole, Topic, TopicValue,
};
use serde_json::json;
use support::device::{GATEWAY, OMP_CONFIG, STEP, Sandbox, run, wait_for};
use support::fake_model::{self as fm, FakeModel, Reply};

#[test]
fn pending_tool_generation_change_still_returns_exactly_once() {
    if std::env::var_os("OMPDECK_TEST_OMP").is_none() {
        eprintln!("skipped: set OMPDECK_TEST_OMP to run the real-omp generation regression");
        return;
    }
    let _omp_override = support::test_omp_override();
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("tr", &config);
    sb.report_failures();
    let model = FakeModel::start(|request| {
        if !fm::has_tools(request) {
            return Reply::text("Fake title");
        }
        if fm::after_last_user(request)
            .iter()
            .any(|message| fm::role(message) == "tool")
        {
            return Reply::text("Routing verdict received.");
        }
        Reply::tool_call(
            "ompdeck_request_review",
            &json!({"summary":"Hold while the controller generation changes"}),
        )
    });
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).unwrap();
    let mut options = GatewayOptions::new(sb.profile.clone(), Role::Gui, Arc::new(|| {}));
    options.autostart = true;
    options.gateway_exe = Some(PathBuf::from(GATEWAY));
    let gui = GatewayClient::start(options);
    wait_for("gateway", STEP, || {
        matches!(gui.link(), Link::Up(_)).then_some(())
    });
    let Done::Session(target, _) = run(
        &gui,
        Mutation::Launch(LaunchSpec {
            agent_child: false,
            mux_name: Some("route".into()),
            cwd: sb.proj(),
            session_path: None,
            label: "Tool result generation".into(),
            argv: vec!["omp".into(), "GENERATION-ROUTING-MARK".into()],
            role: SessionRole::Session,
            copy_of: None,
            view: SessionView::Chat,
        }),
    )
    .unwrap() else {
        panic!("session launch");
    };
    let (request, issuer) = wait_for("pending review", STEP, || {
        gui.events().into_iter().find_map(|event| match event {
            ClientEvent::AgentReviewRequest {
                request,
                target: issuer,
                ..
            } if issuer.key == target.key => Some((request, issuer)),
            _ => None,
        })
    });
    let Done::Session(changed, _) = run(
        &gui,
        Mutation::SetSessionPath {
            target: issuer.clone(),
            path: Some(sb.proj().join("other-conversation.jsonl")),
        },
    )
    .unwrap() else {
        panic!("generation change");
    };
    assert_eq!(changed.key, issuer.key);
    assert_ne!(changed.generation, issuer.generation);
    let completion = wait_for(
        "expired tool verdict on its original live connection",
        STEP,
        || {
            model.find(0, |request| {
                fm::after_last_user(request)
                    .iter()
                    .any(|message| fm::role(message) == "tool")
            })
        },
    );
    let results: Vec<_> = fm::after_last_user(&completion)
        .iter()
        .filter(|message| fm::role(message) == "tool")
        .collect();
    assert_eq!(
        results.len(),
        1,
        "the pending call receives exactly one result"
    );
    assert_eq!(
        fm::text(results[0]),
        "OmpDeck request expired or the session ended"
    );
    wait_for("review cancellation", STEP, || {
        gui.events().into_iter().any(|event|
        matches!(event, ClientEvent::AgentReviewCancel { request: id } if id == request)).then_some(())
    });
    // File following may have advanced the generation once more; kill only this controller.
    let current = wait_for("current generation", STEP, || {
        let Ok(Answer::Topic(value)) = gui.query(Query::Topic(Topic::Controllers)).wait(STEP)?
        else {
            return None;
        };
        let TopicValue::Controllers(controllers) = *value else {
            return None;
        };
        controllers
            .into_iter()
            .find(|controller| controller.target.key == issuer.key)
            .map(|controller| controller.target)
    });
    run(&gui, Mutation::Kill(current)).unwrap();
}

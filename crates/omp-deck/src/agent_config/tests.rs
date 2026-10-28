use super::*;
use deck_core::omp::ModelInfo;

fn models() -> ModelsCache {
    let mut c = ModelsCache::default();
    c.finish(Ok(vec![
        ModelInfo {
            selector: "a/think".into(),
            provider: "a".into(),
            id: "think".into(),
            name: "Thinker".into(),
            thinking: vec!["low".into(), "high".into()],
            ..Default::default()
        },
        ModelInfo {
            selector: "a/plain".into(),
            provider: "a".into(),
            id: "plain".into(),
            name: "Plain".into(),
            ..Default::default()
        },
    ]));
    c
}

#[test]
fn picking_a_model_keeps_a_level_the_new_model_supports() {
    let m = models();
    let switch = |sel: &str, staged: Option<&str>| PickerAction::Switch {
        selector: sel.into(),
        level: staged.map(str::to_owned),
    };
    // No staged level: the agent's high carries over where the target has it.
    assert_eq!(
        model_for(&switch("a/think", None), Some("x/y"), Some("high"), &m).as_deref(),
        Some("a/think:high")
    );
    assert_eq!(
        model_for(&switch("a/plain", None), Some("a/think"), Some("high"), &m).as_deref(),
        Some("a/plain")
    );
    // A staged level wins.
    assert_eq!(
        model_for(&switch("a/think", Some("low")), None, Some("high"), &m).as_deref(),
        Some("a/think:low")
    );
    // Effort pills re-level the current model; without one there is nothing to level.
    assert_eq!(
        model_for(
            &PickerAction::Effort("low".into()),
            Some("a/think"),
            None,
            &m
        )
        .as_deref(),
        Some("a/think:low")
    );
    assert_eq!(
        model_for(&PickerAction::Effort("low".into()), None, None, &m),
        None
    );
    assert_eq!(
        model_for(&PickerAction::Refresh, Some("a/think"), None, &m),
        None
    );
}

#[test]
fn chip_marks_only_catalog_style_models_current() {
    let m = models();
    let l = chip_label(Some("a/think:high"), None, &m, None);
    assert_eq!(
        (l.name.as_str(), l.level.as_deref()),
        ("Thinker", Some("high"))
    );
    assert_eq!(l.current.as_deref(), Some("a/think"));
    let l = chip_label(Some("a/think"), Some("low"), &m, None);
    assert_eq!(l.level.as_deref(), Some("low"));
    for model in ["@smol", "a/think, a/plain"] {
        assert_eq!(
            chip_label(Some(model), None, &m, None).current,
            None,
            "{model}"
        );
    }
    assert_eq!(chip_label(None, None, &m, None).current, None);
}

fn file(path: &str, body: &str, revision: &str) -> AgentFile {
    AgentFile {
        path: PathBuf::from(path),
        name: Some("n".into()),
        description: Some("d".into()),
        model: None,
        thinking_level: None,
        frontmatter: true,
        body: body.into(),
        revision: revision.into(),
        issue: None,
    }
}

fn catalog(files: Vec<AgentFile>) -> Catalog {
    Catalog {
        dirs: vec![deck_core::agent_defs::AgentDir {
            scope: Scope::User,
            dir: PathBuf::from("/u/agents"),
            root: PathBuf::from("/u"),
            agents: files,
            error: None,
        }],
        bare_roots: Vec::new(),
        settings: None,
    }
}

#[test]
fn unsaved_edits_survive_a_reload_and_learn_the_disk_moved() {
    let mut s = AgentsState::default();
    s.loaded(Ok(catalog(vec![file("/u/agents/a.md", "old", "r1")])));
    assert_eq!(s.selected.as_deref(), Some(Path::new("/u/agents/a.md")));
    let mut d = Draft::of(&file("/u/agents/a.md", "old", "r1"));
    d.text = "mine".into();
    s.drafts.insert(PathBuf::from("/u/agents/a.md"), d);
    s.drafts.insert(
        PathBuf::from("/u/agents/b.md"),
        Draft::of(&file("/u/agents/b.md", "b", "rb")),
    );

    s.loaded(Ok(catalog(vec![
        file("/u/agents/a.md", "theirs", "r2"),
        file("/u/agents/b.md", "b2", "rb2"),
    ])));
    let a = &s.drafts[Path::new("/u/agents/a.md")];
    assert_eq!(a.text, "mine");
    assert_eq!(a.disk_revision.as_deref(), Some("r2"));
    // A clean draft simply follows the disk (it is recreated from the new body).
    assert!(!s.drafts.contains_key(Path::new("/u/agents/b.md")));
}

#[test]
fn a_model_edit_moves_the_draft_base_only_when_the_prompt_is_unchanged() {
    let mut s = AgentsState::default();
    let p = PathBuf::from("/u/agents/a.md");
    s.loaded(Ok(catalog(vec![file("/u/agents/a.md", "old", "r1")])));
    let mut d = Draft::of(&file("/u/agents/a.md", "old", "r1"));
    d.text = "mine".into();
    s.drafts.insert(p.clone(), d);
    s.busy = Some((p.clone(), Part::Model));

    s.edit_done(&p, Part::Model, Ok(file("/u/agents/a.md", "old", "r2")));
    assert_eq!(s.drafts[&p].base_revision, "r2");
    assert!(s.busy.is_none());
    s.edit_done(&p, Part::Model, Ok(file("/u/agents/a.md", "other", "r3")));
    assert_eq!(s.drafts[&p].base_revision, "r2");

    s.edit_done(&p, Part::Body, Ok(file("/u/agents/a.md", "mine", "r4")));
    assert!(!s.drafts[&p].dirty());
    assert_eq!(s.drafts[&p].base_revision, "r4");
}

#[test]
fn a_lost_save_race_rereads_and_asks_instead_of_failing() {
    let mut s = AgentsState::default();
    let p = PathBuf::from("/u/agents/a.md");
    s.loaded(Ok(catalog(vec![file("/u/agents/a.md", "old", "r1")])));
    let mut d = Draft::of(&file("/u/agents/a.md", "old", "r1"));
    d.text = "mine".into();
    s.drafts.insert(p.clone(), d);
    let conflict = || Err(format!("x: {}", agent_defs::CHANGED_ON_DISK));

    s.edit_done(&p, Part::Body, conflict());
    assert!(s.want_load, "a conflict rereads the files");
    assert!(
        s.failure.is_none(),
        "the prompt conflict is a choice, not an error"
    );
    s.loaded(Ok(catalog(vec![file("/u/agents/a.md", "theirs", "r2")])));
    assert_eq!(s.drafts[&p].disk_revision.as_deref(), Some("r2"));
    assert_eq!(s.drafts[&p].text, "mine");

    s.edit_done(&p, Part::Model, conflict());
    assert!(s.failure.is_some(), "a lost model pick is reported");
}

fn named(path: &str, name: &str) -> AgentFile {
    AgentFile {
        name: Some(name.into()),
        ..file(path, "b", "r")
    }
}

fn dir(
    scope: Scope,
    dir: &str,
    root: &str,
    agents: Vec<AgentFile>,
) -> deck_core::agent_defs::AgentDir {
    deck_core::agent_defs::AgentDir {
        scope,
        dir: PathBuf::from(dir),
        root: PathBuf::from(root),
        agents,
        error: None,
    }
}

/// User `scout` (a customized copy), project `scout` and `task`, built-ins `scout`, `task`,
/// `sonic`; a second project folder without agents.
fn layered() -> Catalog {
    Catalog {
        dirs: vec![
            dir(
                Scope::User,
                "/u/agents",
                "/u",
                vec![named("/u/agents/scout.md", "scout")],
            ),
            dir(
                Scope::Project,
                "/p/.omp/agents",
                "/p",
                vec![
                    named("/p/.omp/agents/scout.md", "scout"),
                    named("/p/.omp/agents/task.md", "task"),
                ],
            ),
            dir(
                Scope::Bundled,
                "",
                "",
                ["scout", "sonic", "task"]
                    .iter()
                    .map(|n| named(&format!("omp:bundled/{n}.md"), n))
                    .collect(),
            ),
        ],
        bare_roots: vec![PathBuf::from("/q")],
        settings: None,
    }
}

#[test]
fn built_ins_know_what_replaces_them() {
    let l = Listing::new(layered());
    let replaced = |path: &str| l.entry(Path::new(path)).and_then(|e| e.replaced_by.clone());
    // A user file replaces a built-in everywhere, a project file only in its project.
    assert_eq!(
        replaced("omp:bundled/scout.md"),
        Some((PathBuf::from("/u/agents/scout.md"), true))
    );
    assert_eq!(
        replaced("omp:bundled/task.md"),
        Some((PathBuf::from("/p/.omp/agents/task.md"), false))
    );
    assert_eq!(replaced("omp:bundled/sonic.md"), None);
    let relation = |path: &str| {
        l.entry(Path::new(path))
            .and_then(|e| e.relation.clone())
            .unwrap_or_default()
    };
    assert!(relation("/u/agents/scout.md").contains("built-in scout"));
    assert!(relation("/u/agents/scout.md").contains("replaces it in p"));
    assert!(relation("/p/.omp/agents/scout.md").contains("user agent"));

    // New agents go to the user dir or any project folder, one without .omp/agents included.
    let dirs: Vec<_> = l.targets.iter().map(|t| t.dir.clone()).collect();
    assert_eq!(
        dirs,
        [
            PathBuf::from("/u/agents"),
            PathBuf::from("/p/.omp/agents"),
            PathBuf::from("/q/.omp/agents"),
        ]
    );
}

#[test]
fn deleting_a_copy_selects_what_applies_again() {
    let l = Listing::new(layered());
    let user_copy = l.file(Path::new("/u/agents/scout.md")).unwrap().clone();
    let ask = delete_ask(&user_copy, &l);
    assert_eq!(ask.then.as_deref(), Some(Path::new("omp:bundled/scout.md")));
    assert!(ask.after.contains("built-in scout"));
    // In the project, the user's agent of the same name comes back first.
    let project_copy = l
        .file(Path::new("/p/.omp/agents/scout.md"))
        .unwrap()
        .clone();
    let ask = delete_ask(&project_copy, &l);
    assert_eq!(ask.then.as_deref(), Some(Path::new("/u/agents/scout.md")));

    let mut s = AgentsState::default();
    s.loaded(Ok(layered()));
    let gone = PathBuf::from("/u/agents/scout.md");
    s.deleting = Some((gone.clone(), Some(PathBuf::from("omp:bundled/scout.md"))));
    s.deleted(&gone, Err("x".into()));
    assert!(s.failure.is_some() && s.select_after_load.is_none());
    s.deleting = Some((gone.clone(), Some(PathBuf::from("omp:bundled/scout.md"))));
    s.deleted(&gone, Ok(()));
    assert!(s.want_load);
    let mut after = layered();
    after.dirs[0].agents.clear();
    s.loaded(Ok(after));
    assert_eq!(
        s.selected.as_deref(),
        Some(Path::new("omp:bundled/scout.md"))
    );
}

#[test]
fn a_new_agent_is_selected_once_listed_and_a_refusal_stays_in_the_dialog() {
    let mut s = AgentsState::default();
    s.loaded(Ok(layered()));
    let before = s.selected.clone();
    s.create = Some(CreateDialog::new(0));
    if let Some(d) = &mut s.create {
        d.busy = true;
    }
    s.created(Err("exists".into()));
    let d = s.create.as_ref().unwrap();
    assert!(!d.busy && d.error.as_deref() == Some("exists"));

    s.created(Ok(named("/q/.omp/agents/api.md", "api")));
    assert!(s.create.is_none() && s.want_load);
    assert_eq!(
        s.selected, before,
        "kept until the reread lists the new file"
    );
    let mut after = layered();
    after.dirs.insert(
        2,
        dir(
            Scope::Project,
            "/q/.omp/agents",
            "/q",
            vec![named("/q/.omp/agents/api.md", "api")],
        ),
    );
    s.loaded(Ok(after));
    assert_eq!(
        s.selected.as_deref(),
        Some(Path::new("/q/.omp/agents/api.md"))
    );

    // A failed customize is reported on the built-in it was started from.
    s.customizing = Some(PathBuf::from("omp:bundled/sonic.md"));
    s.created(Err("no".into()));
    assert_eq!(
        s.failure,
        Some((PathBuf::from("omp:bundled/sonic.md"), "no".to_owned()))
    );
}

#[test]
fn role_models_show_what_they_stand_for() {
    let m = models();
    let s = AgentSettings::from_values(
        &serde_json::Value::Null,
        &serde_json::Value::Null,
        &serde_json::json!({"smol": "a/think:low"}),
    );
    let l = chip_label(Some("@smol"), Some("high"), &m, Some(&s));
    assert_eq!(l.name, "@smol · Thinker");
    // The agent's thinkingLevel wins over the role's level, as in omp.
    assert_eq!(l.level.as_deref(), Some("high"));
    assert_eq!(l.current, None, "a role is not one catalog model");
    assert_eq!(
        chip_label(Some("@smol"), None, &m, Some(&s))
            .level
            .as_deref(),
        Some("low")
    );
    assert_eq!(chip_label(Some("@slow"), None, &m, Some(&s)).name, "@slow");
}

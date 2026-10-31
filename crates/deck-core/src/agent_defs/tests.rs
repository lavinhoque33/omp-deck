use super::*;

const MEDIUM: &str = "---\n\
name: implement-medium\n\
description: \"Ordinary scoped changes: a feature touching a few files.\"\n\
model: \"anthropic/claude-sonnet-5-5:medium\"\n\
spawns: \"*\"\n\
---\n\
\n\
Worker agent: delegated tasks.\n\
\n\
<directives>\n\
- MUST finish assigned work only.\n\
</directives>\n";

fn p(name: &str) -> PathBuf {
    PathBuf::from("/x/.omp/agents").join(name)
}

#[test]
fn reads_a_definition_like_omp() {
    let a = parse(&p("implement-medium.md"), MEDIUM.as_bytes());
    assert_eq!(a.name.as_deref(), Some("implement-medium"));
    assert_eq!(
        a.description.as_deref(),
        Some("Ordinary scoped changes: a feature touching a few files.")
    );
    assert_eq!(
        a.model.as_deref(),
        Some("anthropic/claude-sonnet-5-5:medium")
    );
    assert!(a.body.starts_with("Worker agent: delegated tasks."));
    assert!(
        a.body.ends_with("</directives>"),
        "body is trimmed: {:?}",
        a.body
    );
    assert!(a.frontmatter);
    assert_eq!(a.issue, None);
    assert_eq!(a.revision, revision(MEDIUM.as_bytes()));
}

#[test]
fn reads_every_model_and_scalar_form() {
    let cases = [
        (
            "model:\n  - a/one\n  - \"b/two:high\"\n",
            "a/one, b/two:high",
        ),
        ("model:\n- a/one\n- b/two\n", "a/one, b/two"),
        ("model: [a/one, 'b/two']\n", "a/one, b/two"),
        ("model: a/one, b/two # fallback\n", "a/one, b/two"),
        ("model: '@smol'\n", "@smol"),
    ];
    for (entry, want) in cases {
        let text = format!("---\nname: n\ndescription: d\n{entry}---\nbody");
        let a = parse(&p("n.md"), text.as_bytes());
        assert_eq!(a.model.as_deref(), Some(want), "{entry:?}");
        assert_eq!(a.body, "body");
    }
    let text = "---\nname: 'it''s'\ndescription: |\n  line one\n  line two\n# note\nthinking-level: high\n---\n";
    let a = parse(&p("n.md"), text.as_bytes());
    assert_eq!(a.name.as_deref(), Some("it's"));
    assert_eq!(a.description.as_deref(), Some("line one\nline two"));
    assert_eq!(a.thinking_level.as_deref(), Some("high"));
    assert_eq!(a.model, None);
    assert_eq!(a.body, "");
}

#[test]
fn leading_comments_and_crlf_are_read_like_omp() {
    let text = "<!-- generated -->\r\n---\r\nname: n\r\ndescription: d\r\n---\r\n\r\nBody\r\n";
    let a = parse(&p("n.md"), text.as_bytes());
    assert_eq!(a.name.as_deref(), Some("n"));
    assert_eq!(a.body, "Body");
    // A comment not followed by frontmatter leaves the document alone.
    let a = parse(&p("n.md"), b"<!-- c -->\nplain");
    assert_eq!(a.issue, Some(Issue::NoFrontmatter));
    assert_eq!(a.body, "<!-- c -->\nplain");
}

#[test]
fn files_omp_skips_say_why() {
    let issue = |text: &str| parse(&p("n.md"), text.as_bytes()).issue;
    assert_eq!(issue("just a prompt"), Some(Issue::NoFrontmatter));
    assert_eq!(issue("---\nname: n\n"), Some(Issue::NoFrontmatter));
    assert_eq!(
        issue("---\ndescription: d\n---\nb"),
        Some(Issue::MissingName)
    );
    assert_eq!(
        issue("---\nname: n\n---\nb"),
        Some(Issue::MissingDescription)
    );
    assert_eq!(
        issue("---\nname: [n]\ndescription: d\n---\nb"),
        Some(Issue::MissingName)
    );
    assert_eq!(
        issue("---\nname: \" Main \"\ndescription: d\n---\nb"),
        Some(Issue::ReservedName)
    );
}

#[test]
fn setting_the_model_touches_only_its_entry() {
    let set =
        |text: &str, m: Option<&str>| apply(text, &Edit::Model(m.map(str::to_owned))).unwrap();
    // Replace: every other byte stays.
    assert_eq!(
        set(MEDIUM, Some("openai/gpt-6:high")),
        MEDIUM.replace(
            "model: \"anthropic/claude-sonnet-5-5:medium\"",
            "model: \"openai/gpt-6:high\""
        )
    );
    // A block list collapses into one entry; the comment after it survives.
    let list = "---\nname: n\nmodel:\n  - a\n  - b\n# keep\ndescription: d\n---\nB\n";
    assert_eq!(
        set(list, Some("c")),
        "---\nname: n\nmodel: \"c\"\n# keep\ndescription: d\n---\nB\n"
    );
    // Remove (inherit the session model), wherever the entry sits.
    assert_eq!(
        set(MEDIUM, None),
        MEDIUM.replace("model: \"anthropic/claude-sonnet-5-5:medium\"\n", "")
    );
    assert_eq!(
        set("---\nname: n\ndescription: d\nmodel: x\n---\nB", None),
        "---\nname: n\ndescription: d\n---\nB"
    );
    // Insert right after the description (a multi-line one included).
    let plain = "---\nname: n\ndescription: >\n  long\n  text\ntools: read\n---\nB\n";
    assert_eq!(
        set(plain, Some("a/b")),
        "---\nname: n\ndescription: >\n  long\n  text\nmodel: \"a/b\"\ntools: read\n---\nB\n"
    );
    assert_eq!(set("---\n---\nB", Some("a")), "---\nmodel: \"a\"\n---\nB");
    // Quotes and backslashes survive the round trip.
    let tricky = set(MEDIUM, Some("we\"ird\\one"));
    let a = parse(&p("n.md"), tricky.as_bytes());
    assert_eq!(a.model.as_deref(), Some("we\"ird\\one"));
}

#[test]
fn model_edits_are_refused_where_they_cannot_apply() {
    assert!(matches!(
        apply("plain prompt", &Edit::Model(Some("a".into()))),
        Err(EditError::NoFrontmatter)
    ));
    for bad in ["", "  ", "a\nb"] {
        assert!(matches!(
            apply(MEDIUM, &Edit::Model(Some(bad.into()))),
            Err(EditError::BadModel)
        ));
    }
}

#[test]
fn replacing_the_body_keeps_the_frontmatter_and_spacing() {
    let out = apply(MEDIUM, &Edit::Body("\n  New prompt.\n\n".into())).unwrap();
    let head = &MEDIUM[..MEDIUM.find("Worker").unwrap()];
    assert_eq!(out, format!("{head}New prompt.\n"));
    let a = parse(&p("n.md"), out.as_bytes());
    assert_eq!(a.body, "New prompt.");
    assert_eq!(
        a.model.as_deref(),
        Some("anthropic/claude-sonnet-5-5:medium")
    );

    let empty = "---\nname: n\ndescription: d\n---";
    assert_eq!(
        apply(empty, &Edit::Body("Hi".into())).unwrap(),
        "---\nname: n\ndescription: d\n---\n\nHi\n"
    );
    assert_eq!(apply("old", &Edit::Body("new".into())).unwrap(), "new");
    // CRLF files stay CRLF.
    let crlf = MEDIUM.replace('\n', "\r\n");
    let out = apply(&crlf, &Edit::Body("a\nb".into())).unwrap();
    assert!(!out.replace("\r\n", "").contains('\n'), "{out:?}");
    assert!(out.ends_with("a\r\nb\r\n"));
}

#[test]
fn discovery_follows_omp_precedence_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("home/.omp/agent");
    fs::create_dir_all(agent.join("agents")).unwrap();
    fs::write(
        agent.join("agents/u.md"),
        "---\nname: u\ndescription: d\n---\nU",
    )
    .unwrap();
    fs::write(agent.join("agents/notes.txt"), "ignored").unwrap();

    let repo = tmp.path().join("repo");
    let agents = repo.join(".omp/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::create_dir_all(repo.join("crates/a")).unwrap();
    fs::write(
        agents.join("b.md"),
        "---\nname: same\ndescription: d\n---\nB",
    )
    .unwrap();
    fs::write(
        agents.join("a.md"),
        "---\nname: same\ndescription: d\n---\nA",
    )
    .unwrap();
    let bare = tmp.path().join("bare");
    fs::create_dir_all(&bare).unwrap();

    let cat = discover(
        &agent,
        &[
            repo.join("crates/a"),
            repo.clone(),
            bare.clone(),
            tmp.path().join("gone"),
        ],
    );
    assert_eq!(cat.dirs.len(), 2, "{cat:#?}");
    // Folders no .omp/agents applies to, where a project agent could be created.
    assert_eq!(cat.bare_roots, vec![bare]);
    assert_eq!(cat.dirs[0].scope, Scope::User);
    assert_eq!(cat.dirs[0].agents.len(), 1);
    let project = &cat.dirs[1];
    assert_eq!(project.scope, Scope::Project);
    assert_eq!(project.root, repo);
    let names: Vec<_> = project
        .agents
        .iter()
        .map(|a| (a.path.file_name().unwrap().to_owned(), a.issue.clone()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("a.md".into(), None),
            ("b.md".into(), Some(Issue::DuplicateOf(agents.join("a.md")))),
        ]
    );
    // A user dir that does not exist yet is still listed (where new agents would go).
    let cat = discover(&tmp.path().join("nowhere"), &[]);
    assert_eq!(cat.dirs.len(), 1);
    assert!(cat.dirs[0].agents.is_empty() && cat.dirs[0].error.is_none());
}

#[test]
fn save_checks_the_revision_and_the_path() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let dir = agent.join("agents");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("m.md");
    fs::write(&file, MEDIUM).unwrap();
    let seen = read_file(&file);

    let saved = save(
        &file,
        &agent,
        &seen.revision,
        &Edit::Model(Some("x/y".into())),
    )
    .unwrap();
    assert_eq!(saved.model.as_deref(), Some("x/y"));
    assert_eq!(saved.revision, revision(&fs::read(&file).unwrap()));
    assert_eq!(saved.body, seen.body);

    // The revision read before the first save is stale now: nothing is written.
    let before = fs::read(&file).unwrap();
    assert!(matches!(
        save(&file, &agent, &seen.revision, &Edit::Body("lost".into())),
        Err(EditError::Changed)
    ));
    assert_eq!(fs::read(&file).unwrap(), before);

    // Only `.md` files of an agents directory are writable.
    let other = tmp.path().join("notes.md");
    fs::write(&other, MEDIUM).unwrap();
    let rev = revision(MEDIUM.as_bytes());
    assert!(matches!(
        save(&other, &agent, &rev, &Edit::Body("x".into())),
        Err(EditError::NotAgentFile(_))
    ));
    let txt = dir.join("m.txt");
    fs::write(&txt, MEDIUM).unwrap();
    assert!(matches!(
        save(&txt, &agent, &rev, &Edit::Body("x".into())),
        Err(EditError::NotAgentFile(_))
    ));
    assert!(is_agent_file(&file, &agent));
    assert!(!is_agent_file(&dir.join("missing.md"), &agent));
}

#[cfg(unix)]
#[test]
fn save_writes_through_a_symlink_and_keeps_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let dir = agent.join("agents");
    fs::create_dir_all(&dir).unwrap();
    let real = tmp.path().join("dotfiles/m.md");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, MEDIUM).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
    let link = dir.join("m.md");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let seen = read_file(&link);
    save(&link, &agent, &seen.revision, &Edit::Body("Linked".into())).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read_file(&real).body, "Linked");
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let leftovers: Vec<_> = fs::read_dir(real.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec![std::ffi::OsString::from("m.md")]);
}

#[test]
fn settings_writes_change_only_the_agent_they_name() {
    let s = AgentSettings::from_values(
        &serde_json::json!({"a": "x/y:high", "b": ["p", " q "], "c": "  ", "d": 3}),
        &serde_json::json!(["sonic", 1, "scout"]),
        &serde_json::json!({"smol": "f/fast:low", "slow": " ", "n": 2}),
    );
    assert_eq!(s.override_for("a").as_deref(), Some("x/y:high"));
    assert_eq!(s.override_for("b").as_deref(), Some("p, q"));
    assert_eq!(s.override_for("c"), None);
    assert_eq!(s.override_for("d"), None);
    assert!(s.is_disabled("sonic") && !s.is_disabled("a"));
    // Entries this page cannot read are written back as configured.
    assert_eq!(
        s.overrides_with("a", None),
        serde_json::json!({"b": ["p", " q "], "c": "  ", "d": 3})
    );
    assert_eq!(
        s.overrides_with("b", Some("m/n")),
        serde_json::json!({"a": "x/y:high", "b": "m/n", "c": "  ", "d": 3})
    );
    assert_eq!(
        s.disabled_with("task", true),
        serde_json::json!(["sonic", "scout", "task"])
    );
    assert_eq!(
        s.disabled_with("sonic", false),
        serde_json::json!(["scout"])
    );
    assert_eq!(
        s.disabled_with("scout", true),
        serde_json::json!(["sonic", "scout"])
    );
    assert_eq!(s.role_model("@smol"), Some("f/fast:low"));
    assert_eq!(s.role_model("pi/smol:high"), Some("f/fast:low"));
    assert_eq!(s.role_model("@slow"), None);
    assert_eq!(s.role_model("smol"), None);
}

/// What `omp agents unpack` writes for scout (abridged).
const SCOUT: &str = "---\nname: scout\ndescription: \"Fast read-only scout\"\ntools:\n  - read\n  - grep\nmodel:\n  - \"@smol\"\nthinkingLevel: medium\n---\n\nYou are a scout.\n";

#[test]
fn built_ins_parse_like_files_under_a_pseudo_path() {
    let dir = bundled(&[
        (
            "task.md".into(),
            "---\nname: task\ndescription: d\n---\nT".into(),
        ),
        ("scout.md".into(), SCOUT.into()),
    ]);
    assert_eq!(dir.scope, Scope::Bundled);
    let names: Vec<_> = dir.agents.iter().map(AgentFile::display_name).collect();
    assert_eq!(names, ["scout", "task"]);
    let scout = &dir.agents[0];
    assert_eq!(scout.model.as_deref(), Some("@smol"));
    assert_eq!(scout.thinking_level.as_deref(), Some("medium"));
    assert!(is_bundled(&scout.path) && !is_bundled(&p("scout.md")));
    // Never a writable file, whatever exists on disk.
    assert!(!is_agent_file(&scout.path, Path::new("/")));
}

#[test]
fn new_agents_are_written_once_where_omp_reads_them() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let user = agent.join("agents");
    let new = NewAgent {
        name: "api-reviewer".into(),
        description: "Reviews \"API\" changes\nacross crates".into(),
        model: Some("a/b:high".into()),
        body: "\n  Review the API.\n".into(),
    };
    // The user dir is created on first use.
    let made = create(&user, &agent, &new.name, &new.content().unwrap()).unwrap();
    assert_eq!(made.path, user.join("api-reviewer.md"));
    assert_eq!(made.issue, None);
    assert_eq!(
        made.description.as_deref(),
        Some("Reviews \"API\" changes\nacross crates")
    );
    assert_eq!(made.model.as_deref(), Some("a/b:high"));
    assert_eq!(made.body, "Review the API.");
    assert_eq!(read_file(&made.path), made);

    // Never replaces a file, nor adds a second definition of a name.
    let before = fs::read(&made.path).unwrap();
    assert!(matches!(
        create(&user, &agent, "api-reviewer", SCOUT),
        Err(EditError::Exists(p)) if p == made.path
    ));
    fs::write(
        user.join("other.md"),
        "---\nname: scout\ndescription: d\n---\n",
    )
    .unwrap();
    assert!(matches!(
        create(&user, &agent, "scout", SCOUT),
        Err(EditError::Exists(p)) if p == user.join("other.md")
    ));
    assert_eq!(fs::read(&made.path).unwrap(), before);

    // A project's .omp/agents is created inside an existing project only.
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let project = repo.join(".omp/agents");
    let copied = create(&project, &agent, "scout", SCOUT).unwrap();
    assert_eq!(fs::read_to_string(&copied.path).unwrap(), SCOUT);
    for dir in [
        tmp.path().join("gone/.omp/agents"),
        tmp.path().join("repo/agents"),
        PathBuf::from("rel/.omp/agents"),
    ] {
        assert!(
            matches!(
                create(&dir, &agent, "x", SCOUT),
                Err(EditError::NotAgentsDir(_))
            ),
            "{dir:?}"
        );
    }
    assert!(!tmp.path().join("gone").exists());
}

#[test]
fn new_agent_names_and_fields_are_checked() {
    for ok in ["a", "api-reviewer", "v2-scout"] {
        assert!(check_name(ok).is_ok(), "{ok}");
    }
    for bad in [
        "", "Api", "a b", "a_b", "-a", "a-", "a--b", "../x", "main", "sub", "a.md",
    ] {
        assert!(
            matches!(check_name(bad), Err(EditError::BadName(_))),
            "{bad:?}"
        );
    }
    assert!(check_name(&"a".repeat(NAME_MAX + 1)).is_err());
    let new = |description: &str, model: Option<&str>| NewAgent {
        name: "n".into(),
        description: description.into(),
        model: model.map(str::to_owned),
        body: String::new(),
    };
    assert!(matches!(
        new("  ", None).content(),
        Err(EditError::NoDescription)
    ));
    assert!(matches!(
        new("d", Some(" ")).content(),
        Err(EditError::BadModel)
    ));
    // No prompt yet: the frontmatter alone, which omp loads with an empty prompt.
    let bare = new("d", None).content().unwrap();
    assert_eq!(bare, "---\nname: n\ndescription: \"d\"\n---\n");
    assert_eq!(parse(&p("n.md"), bare.as_bytes()).issue, None);
}

#[test]
fn delete_checks_the_revision_and_the_path() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let dir = agent.join("agents");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("m.md");
    fs::write(&file, MEDIUM).unwrap();
    assert!(matches!(
        delete(&file, &agent, "stale"),
        Err(EditError::Changed)
    ));
    let outside = tmp.path().join("m.md");
    fs::write(&outside, MEDIUM).unwrap();
    assert!(matches!(
        delete(&outside, &agent, &revision(MEDIUM.as_bytes())),
        Err(EditError::NotAgentFile(_))
    ));
    assert!(file.exists() && outside.exists());
    delete(&file, &agent, &revision(MEDIUM.as_bytes())).unwrap();
    assert!(!file.exists());
}

//! Behaviour tests of the tree window: the keyboard map, the summary dialog, the label
//! editor, search confinement, variable-height geometry, and the preview request lifecycle.

use super::geometry::{row_height, row_slot, visible_range};
use super::keys::{Move, match_target, move_target, nearest_match};
use super::*;
use crate::tree_layout::RowClass;
use egui::{Key, Pos2};

#[test]
fn gutter_compresses_wide_trees_only() {
    assert_eq!(gutter(1), (18.0, 18.0));
    assert_eq!(gutter(7), (18.0, 7.0 * 18.0));
    let (step, width) = gutter(14);
    assert!(step < 18.0 && (width - 7.0 * 18.0).abs() < 0.01);
    assert!(gutter(400).0 >= 6.0);
}

#[test]
fn selection_steps_clamp_at_both_ends() {
    assert_eq!(step_row(Some(3), 1, 5), Some(4));
    assert_eq!(step_row(Some(4), 1, 5), Some(4));
    assert_eq!(step_row(Some(1), -12, 5), Some(0));
    assert_eq!(step_row(Some(1), 12, 5), Some(4));
    assert_eq!(step_row(None, 1, 5), Some(0));
    assert_eq!(step_row(None, -1, 5), Some(4));
    assert_eq!(step_row(Some(0), 1, 0), None);
}

#[test]
fn tool_rows_are_shorter_with_a_gap_between_cards() {
    assert_eq!(row_height(RowClass::Tool), 28.0);
    assert_eq!(row_height(RowClass::Assistant), 38.0);
    assert_eq!(row_height(RowClass::User), 38.0);
    assert_eq!(row_slot(RowClass::Tool), row_height(RowClass::Tool) + 5.0);
}

#[test]
fn visible_rows_come_from_the_prefix_sums() {
    // Three message slots (43 each) then a tool slot (33).
    let offsets = vec![0.0, 43.0, 86.0, 129.0, 162.0];
    assert_eq!(visible_range(&offsets, 0.0, 50.0), (0, 3));
    assert_eq!(visible_range(&offsets, 44.0, 90.0), (1, 4));
    assert_eq!(visible_range(&offsets, 130.0, 200.0), (3, 4));
    assert_eq!(visible_range(&offsets, 500.0, 600.0), (4, 4));
    assert_eq!(visible_range(&[0.0], 0.0, 50.0), (0, 0));
}

#[test]
fn row_centres_are_card_centres() {
    // Two message rows: centres at 19 and 43+19.
    let offsets = vec![0.0, 43.0, 86.0];
    let geo = Geometry::new(0.0, 0.0, 18.0, 18.0, 400.0, &offsets);
    assert_eq!(geo.y(0), 19.0);
    assert_eq!(geo.y(1), 62.0);
    let card = geo.card(0, RowClass::Assistant);
    assert_eq!((card.top(), card.height()), (0.0, 38.0));
    let tool = geo.card(1, RowClass::Tool);
    assert_eq!(tool.height(), 28.0);
    assert!(
        tool.left() > card.left(),
        "tool cards sit further right, still on their lane's node"
    );
}

/// A chain of `n` alternating user / assistant nodes.
fn chain(n: usize) -> SessionTree {
    let nodes: Vec<Arc<deck_core::tree::TreeNode>> = (0..n)
        .map(|i| {
            Arc::new(deck_core::tree::TreeNode {
                id: format!("n{i}"),
                parent: i.checked_sub(1).map(|p| format!("n{p}")),
                kind: if i % 2 == 0 {
                    NodeKind::User
                } else {
                    NodeKind::Assistant
                },
                timestamp: None,
                text: String::new(),
                has_text: true,
                stop_reason: None,
                active_leaf: None,
                cost_before: Default::default(),
                search_text: String::new(),
                attribution: None,
                user_request: i % 2 == 0,
                tool_name: None,
                line: None,
            })
        })
        .collect();
    SessionTree {
        index: nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (Arc::from(n.id.as_str()), i))
            .collect(),
        children: (0..n)
            .map(|i| Arc::new(if i + 1 < n { vec![i + 1] } else { Vec::new() }))
            .collect(),
        roots: vec![0],
        nodes,
        ..SessionTree::default()
    }
}

#[test]
fn page_keys_move_by_the_visible_row_count_and_clamp() {
    let t = chain(30);
    let l = tree_layout::build(&t, Some(29), Filter::Conversation);
    let go = |cur, mv| move_target(&t, &l, Some(cur), mv, 7);
    assert_eq!(go(10, Move::Page(1)), Some(17));
    assert_eq!(go(10, Move::Page(-1)), Some(3));
    assert_eq!(go(25, Move::Page(1)), Some(29));
    assert_eq!(go(2, Move::Page(-1)), Some(0));
    assert_eq!(go(10, Move::Step(1)), Some(11));
    assert_eq!(go(10, Move::Home), Some(0));
    assert_eq!(go(10, Move::End), Some(29));
    // A list that has not been measured (page 0) still moves by one row.
    assert_eq!(move_target(&t, &l, Some(10), Move::Page(1), 0), Some(11));
    // Jumps need a selection.
    assert_eq!(
        move_target(&t, &l, None, Move::Turn { forward: true }, 7),
        None
    );
    assert_eq!(go(10, Move::Turn { forward: true }), Some(11));
    assert_eq!(go(10, Move::Branch { forward: true }), None);
}

#[test]
fn going_to_the_current_leaf_says_so_and_a_blocked_session_says_why() {
    let t = chain(3);
    let l = tree_layout::build(&t, Some(2), Filter::Conversation);
    let input = |navigate| TreeInput {
        navigate,
        ..plain_input()
    };
    assert_eq!(
        go_blocked(&l, &input(Ok(())), 2),
        Some("Already at this point")
    );
    // The leaf here is a user prompt: omp still says "Already at this point" for it, and
    // switches only for another entry.
    assert!(matches!(t.nodes[l.rows[2].node].kind, NodeKind::User));
    assert_eq!(go_blocked(&l, &input(Ok(())), 1), None);
    assert_eq!(
        go_blocked(&l, &input(Err("Busy")), 2),
        Some("Busy"),
        "the session's own reason wins"
    );
}

fn plain_input<'a>() -> TreeInput<'a> {
    TreeInput {
        tree: None,
        revision: 1,
        error: None,
        session: None,
        live_leaf: None,
        navigate: Ok(()),
        fork: Ok(()),
        summary_prompt: false,
        label: Ok(()),
        reanswer: Ok(()),
        busy: None,
        summarizing: false,
        now: 0,
    }
}

/// The window over a tree, driven frame by frame.
struct Window {
    ctx: egui::Context,
    st: TreeState,
    tree: Arc<SessionTree>,
}

impl Window {
    fn new(tree: SessionTree) -> Window {
        let ctx = egui::Context::default();
        theme::apply(
            &ctx,
            deck_core::config::UiStyle::Neutral,
            deck_core::config::Density::Compact,
            UiFont::Auto,
            true,
        );
        let mut w = Window {
            ctx,
            st: TreeState::new("t".to_owned()),
            tree: Arc::new(tree),
        };
        w.frame(Vec::new(), |_| {});
        w
    }

    fn frame(
        &mut self,
        events: Vec<egui::Event>,
        tweak: impl FnOnce(&mut TreeInput<'_>),
    ) -> Vec<TreeAction> {
        let tree = Arc::clone(&self.tree);
        let mut input = plain_input();
        input.tree = Some(&tree);
        tweak(&mut input);
        let raw = egui::RawInput {
            events,
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
            ..Default::default()
        };
        let mut out = Vec::new();
        let st = &mut self.st;
        let mut full = self.ctx.run_ui(raw, |ui| {
            out = show(ui.ctx(), st, &input);
        });
        full.textures_delta.clear();
        out
    }

    fn key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        tweak: impl FnOnce(&mut TreeInput<'_>),
    ) -> Vec<TreeAction> {
        let event = egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        };
        self.frame(vec![event], tweak)
    }

    /// Selects the first row (the tree has no leaf, so nothing is selected at first).
    fn select_first(&mut self) {
        self.key(Key::ArrowDown, Modifiers::NONE, |_| {});
    }
}

#[test]
fn go_here_asks_about_a_summary_only_when_omp_does_and_shift_enter_skips_the_question() {
    use BranchSummary::*;
    assert_eq!(go_plan(false, false), GoPlan::Now(None));
    assert_eq!(go_plan(true, false), GoPlan::Ask);
    assert_eq!(go_plan(false, true), GoPlan::Now(Summarize));
    assert_eq!(go_plan(true, true), GoPlan::Now(Summarize));
}

#[test]
fn the_summary_answer_maps_to_what_is_sent() {
    let mut d = SummaryDialog::new("e".to_owned(), false);
    assert_eq!(d.summary(), BranchSummary::None);
    d.choose(SummaryChoice::Summarize);
    assert_eq!(d.summary(), BranchSummary::Summarize);
    d.choose(SummaryChoice::Custom);
    assert!(
        d.focus_editor,
        "picking the custom prompt moves to its field"
    );
    // Blank instructions are omp's default prompt.
    d.instructions = "  \n ".to_owned();
    assert_eq!(d.summary(), BranchSummary::Summarize);
    d.instructions = "  focus on the tests \n".to_owned();
    assert_eq!(
        d.summary(),
        BranchSummary::Custom("focus on the tests".to_owned())
    );
    // The arrow keys stop at both ends.
    assert_eq!(SummaryChoice::None.step(-1), SummaryChoice::None);
    assert_eq!(SummaryChoice::None.step(1), SummaryChoice::Summarize);
    assert_eq!(SummaryChoice::Custom.step(1), SummaryChoice::Custom);
}

#[test]
fn enter_goes_straight_away_and_the_summary_question_is_answered_in_its_dialog() {
    let mut w = Window::new(chain(3));
    w.select_first();
    let a = w.key(Key::Enter, Modifiers::NONE, |_| {});
    assert!(
        matches!(&a[..], [TreeAction::Navigate { entry, user: true, summary: BranchSummary::None }] if entry == "n0"),
        "{}",
        a.len()
    );

    // omp asks (setting on): nothing is sent until the dialog is answered.
    let on = |i: &mut TreeInput<'_>| i.summary_prompt = true;
    assert!(w.key(Key::Enter, Modifiers::NONE, on).is_empty());
    assert!(w.st.summary.is_some());
    // Down picks "Summarize"; Enter confirms it.
    assert!(w.key(Key::ArrowDown, Modifiers::NONE, on).is_empty());
    let a = w.key(Key::Enter, Modifiers::NONE, on);
    assert!(
        matches!(&a[..], [TreeAction::Navigate { entry, summary: BranchSummary::Summarize, .. }] if entry == "n0")
    );
    assert!(w.st.summary.is_none());
}

#[test]
fn shift_enter_summarizes_without_asking_whatever_the_setting() {
    let mut w = Window::new(chain(3));
    w.select_first();
    for on in [false, true] {
        let a = w.key(Key::Enter, Modifiers::SHIFT, |i| i.summary_prompt = on);
        assert!(
            matches!(
                &a[..],
                [TreeAction::Navigate {
                    summary: BranchSummary::Summarize,
                    ..
                }]
            ),
            "setting {on}"
        );
        assert!(w.st.summary.is_none());
    }
}

#[test]
fn esc_in_the_summary_dialog_cancels_the_dialog_not_the_tree() {
    let mut w = Window::new(chain(3));
    w.select_first();
    let on = |i: &mut TreeInput<'_>| i.summary_prompt = true;
    w.key(Key::Enter, Modifiers::NONE, on);
    assert!(w.st.summary.is_some());
    let a = w.key(Key::Escape, Modifiers::NONE, on);
    assert!(a.is_empty(), "no Close, no Navigate");
    assert!(w.st.summary.is_none());
    // A second Esc is the tree's own: it closes.
    let a = w.key(Key::Escape, Modifiers::NONE, on);
    assert!(matches!(&a[..], [TreeAction::Close]));
}

#[test]
fn ctrl_c_closes_the_tree_like_omp() {
    let mut w = Window::new(chain(3));
    w.select_first();
    let a = w.key(Key::C, Modifiers::COMMAND, |_| {});
    assert!(matches!(&a[..], [TreeAction::Close]));
    // …but not while the label editor owns the keys.
    let mut w = Window::new(chain(3));
    w.select_first();
    w.key(Key::L, Modifiers::NONE, |_| {});
    assert!(w.st.label.is_some());
    let a = w.key(Key::C, Modifiers::COMMAND, |_| {});
    assert!(a.is_empty(), "the editor keeps its keys");
    assert!(w.st.label.is_some());
    // …and not while the summary dialog owns the keys.
    let mut w = Window::new(chain(3));
    w.select_first();
    let on = |i: &mut TreeInput<'_>| i.summary_prompt = true;
    w.key(Key::Enter, Modifiers::NONE, on);
    assert!(w.st.summary.is_some());
    let a = w.key(Key::C, Modifiers::COMMAND, on);
    assert!(a.is_empty());
    assert!(w.st.summary.is_some());
}

#[test]
fn esc_while_summarizing_cancels_the_summary_not_the_tree() {
    let mut w = Window::new(chain(3));
    w.select_first();
    let busy = |i: &mut TreeInput<'_>| {
        i.busy = Some("Summarizing the branch… this can take a while");
        i.summarizing = true;
    };
    let a = w.key(Key::Escape, Modifiers::NONE, busy);
    assert!(matches!(&a[..], [TreeAction::CancelSummary]));
    // Other busy states keep Esc as close.
    let busy = |i: &mut TreeInput<'_>| {
        i.busy = Some("Moving to the entry…");
        i.summarizing = false;
    };
    let a = w.key(Key::Escape, Modifiers::NONE, busy);
    assert!(matches!(&a[..], [TreeAction::Close]));
}

#[test]
fn the_label_editor_saves_a_trimmed_label_removes_on_blank_and_cancels_on_esc() {
    let mut t = chain(3);
    t.labels.insert("n0".to_owned(), "old".to_owned());
    let mut w = Window::new(t);
    w.select_first();
    let edit = |w: &mut Window, text: &str| {
        assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
        let e = w.st.label.as_mut().expect("the editor opens");
        text.clone_into(&mut e.text);
    };

    // Opens prefilled with the current label.
    assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
    let e = w.st.label.as_ref().expect("the editor opens");
    assert_eq!((e.entry.as_str(), e.text.as_str()), ("n0", "old"));
    w.key(Key::Escape, Modifiers::NONE, |_| {});
    assert!(w.st.label.is_none());

    edit(&mut w, "  new  ");
    let a = w.key(Key::Enter, Modifiers::NONE, |_| {});
    assert!(
        matches!(&a[..], [TreeAction::SetLabel { entry, label: Some(l) }] if entry == "n0" && l == "new")
    );
    assert!(w.st.label.is_none());

    // Blank removes.
    edit(&mut w, "   ");
    let a = w.key(Key::Enter, Modifiers::NONE, |_| {});
    assert!(matches!(&a[..], [TreeAction::SetLabel { entry, label: None }] if entry == "n0"));

    // Esc discards the typing and is not the tree's Esc.
    edit(&mut w, "discarded");
    let a = w.key(Key::Escape, Modifiers::NONE, |_| {});
    assert!(a.is_empty());
    assert!(w.st.label.is_none());

    // Saving what is already there sends nothing.
    assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
    assert!(w.key(Key::Enter, Modifiers::NONE, |_| {}).is_empty());
    assert!(w.st.label.is_none());
}

#[test]
fn shift_l_opens_the_label_editor_like_plain_l() {
    let mut w = Window::new(chain(3));
    w.select_first();
    assert!(w.key(Key::L, Modifiers::SHIFT, |_| {}).is_empty());
    let e = w.st.label.as_ref().expect("Shift+L opens the editor");
    assert_eq!(e.entry.as_str(), "n0");
}

#[test]
fn the_label_key_waits_for_an_empty_search_and_an_available_session() {
    let mut w = Window::new(chain(3));
    w.select_first();
    // omp types into its search; here a typed query keeps `L` out of the way.
    w.st.search = "n".to_owned();
    assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
    assert!(w.st.label.is_none());
    assert!(w.key(Key::L, Modifiers::SHIFT, |_| {}).is_empty());
    assert!(w.st.label.is_none());
    w.st.search.clear();
    // A refused gate says why instead of opening the editor.
    let a = w.key(Key::L, Modifiers::NONE, |i| {
        i.label = Err("Another tree action is still running");
    });
    assert!(
        matches!(&a[..], [TreeAction::Notice(t)] if t == "Another tree action is still running")
    );
    assert!(w.st.label.is_none());
}

#[test]
fn label_edit_states() {
    let mut e = LabelEdit::new("x".to_owned(), None);
    assert_eq!(e.finish(), LabelEnd::Unchanged);
    e.text = "  ".to_owned();
    assert_eq!(
        e.finish(),
        LabelEnd::Unchanged,
        "blank over nothing is nothing"
    );
    e.text = " tag ".to_owned();
    assert_eq!(e.finish(), LabelEnd::Set(Some("tag".to_owned())));
    let mut e = LabelEdit::new("x".to_owned(), Some("tag"));
    e.text = "tag ".to_owned();
    assert_eq!(e.finish(), LabelEnd::Unchanged);
    e.text.clear();
    assert_eq!(e.finish(), LabelEnd::Set(None));
}

#[test]
fn omp_initial_filter_applies_only_until_the_user_picks_one() {
    let mut st = TreeState::new("t".to_owned());
    assert_eq!(st.filter, Filter::Conversation);
    st.set_initial_filter(Filter::All);
    assert_eq!(st.filter, Filter::All);
    // The user picks: the initial value no longer applies.
    set_filter(&mut st, Filter::UserOnly);
    assert_eq!(st.filter, Filter::UserOnly);
    st.set_initial_filter(Filter::All);
    assert_eq!(st.filter, Filter::UserOnly);
}

#[test]
fn re_answer_is_offered_only_on_an_ask_result_and_needs_a_live_idle_tab() {
    let mut t = chain(3);
    {
        let n = Arc::make_mut(&mut t.nodes[1]);
        n.kind = NodeKind::ToolResult;
        n.text = "[ask: {\"questions\":[…]}]".to_owned();
        n.tool_name = Some("ask".to_owned());
    }
    let l = tree_layout::build(&t, Some(2), Filter::All);
    let live = plain_input();
    // Row 0 is a prompt, row 1 the ask result.
    assert!(reanswer_blocked(&t, &l, &live, 0).is_some());
    assert_eq!(reanswer_blocked(&t, &l, &live, 1), None);
    let mut off = plain_input();
    off.reanswer = Err("omp is busy — wait for it to finish");
    assert_eq!(
        reanswer_blocked(&t, &l, &off, 1),
        Some("omp is busy — wait for it to finish")
    );

    // The action carries the entry id, or the reason.
    let mut w = Window::new(t);
    w.st.filter = Filter::All;
    w.frame(Vec::new(), |_| {});
    let tree = Arc::clone(&w.tree);
    let mut out = Vec::new();
    reanswer_here(&w.st, &tree, &live, 1, &mut out);
    assert!(matches!(&out[..], [TreeAction::Reanswer { entry }] if entry == "n1"));
    out.clear();
    reanswer_here(&w.st, &tree, &off, 1, &mut out);
    assert!(matches!(&out[..], [TreeAction::Notice(_)]));
    // The whole list is shown: row 1 is the ask result, row 0 is not.
    out.clear();
    reanswer_here(&w.st, &tree, &live, 0, &mut out);
    assert!(matches!(&out[..], [TreeAction::Notice(_)]));
}

#[test]
fn the_nearest_match_is_the_selection_itself_then_the_next_one_then_the_first() {
    let rows = [false, true, false, true, false];
    assert_eq!(nearest_match(&rows, Some(1)), Some(1));
    assert_eq!(nearest_match(&rows, Some(2)), Some(3));
    assert_eq!(
        nearest_match(&rows, Some(4)),
        Some(1),
        "nothing after it: the first"
    );
    assert_eq!(nearest_match(&rows, None), Some(1));
    assert_eq!(nearest_match(&[false, false], Some(0)), None);
}

#[test]
fn moves_skip_the_rows_a_query_excludes_and_stop_at_the_ends() {
    let rows = [false, true, false, true, true, false];
    let go = |cur, mv| match_target(&rows, cur, mv, 2);
    assert_eq!(go(Some(1), Move::Step(1)), Some(3));
    assert_eq!(go(Some(3), Move::Step(1)), Some(4));
    assert_eq!(go(Some(4), Move::Step(1)), Some(4), "no match below");
    assert_eq!(go(Some(3), Move::Step(-1)), Some(1));
    assert_eq!(go(Some(1), Move::Step(-1)), Some(1), "no match above");
    // From an excluded row the first step lands on the neighbouring match.
    assert_eq!(go(Some(2), Move::Step(1)), Some(3));
    assert_eq!(go(Some(2), Move::Step(-1)), Some(1));
    assert_eq!(go(Some(0), Move::Step(-1)), Some(1));
    // A page walks that many matches; the ends are the first and last match.
    assert_eq!(go(Some(1), Move::Page(1)), Some(4));
    assert_eq!(go(Some(4), Move::Page(-1)), Some(1));
    assert_eq!(go(Some(3), Move::Home), Some(1));
    assert_eq!(go(Some(3), Move::End), Some(4));
    assert_eq!(go(None, Move::Step(1)), Some(1));
    assert_eq!(go(None, Move::Step(-1)), Some(4));
    // Jumps are not list walks; nothing matching means no move at all.
    assert_eq!(go(Some(1), Move::Turn { forward: true }), None);
    assert_eq!(
        match_target(&[false, false], Some(0), Move::Step(1), 2),
        None
    );
}

#[test]
fn a_query_confines_selection_and_go_here_to_its_matches() {
    let mut t = chain(6);
    for (i, n) in t.nodes.iter_mut().enumerate() {
        Arc::make_mut(n).search_text = if i % 2 == 1 { "alpha" } else { "zeta" }.to_owned();
    }
    let mut w = Window::new(t);
    w.select_first();
    assert_eq!(w.st.selected.as_deref(), Some("n0"));
    let nothing = |_: &mut TreeInput<'_>| {};

    // Typing a query that excludes the selection moves it to the first match after it.
    w.st.search = "alpha".to_owned();
    w.frame(Vec::new(), nothing);
    assert_eq!(w.st.selected.as_deref(), Some("n1"));

    // Up / Down only visit matches.
    w.key(Key::ArrowDown, Modifiers::NONE, nothing);
    assert_eq!(w.st.selected.as_deref(), Some("n3"));
    w.key(Key::ArrowUp, Modifiers::NONE, nothing);
    w.key(Key::ArrowUp, Modifiers::NONE, nothing);
    assert_eq!(w.st.selected.as_deref(), Some("n1"));

    // Enter acts on the match.
    let a = w.key(Key::Enter, Modifiers::NONE, nothing);
    assert!(matches!(&a[..], [TreeAction::Navigate { entry, .. }] if entry == "n1"));

    // A row the query excludes (a click on a dimmed row) is not a place to go.
    w.st.selected = Some("n2".to_owned());
    assert!(w.key(Key::Enter, Modifiers::NONE, nothing).is_empty());
    assert!(w.key(Key::Enter, Modifiers::SHIFT, nothing).is_empty());
    assert_eq!(
        w.st.selected.as_deref(),
        Some("n2"),
        "the click's selection stays"
    );

    // A new query that excludes the selection moves it on, forward first.
    w.st.selected = Some("n3".to_owned());
    w.st.search = "zeta".to_owned();
    w.frame(Vec::new(), nothing);
    assert_eq!(w.st.selected.as_deref(), Some("n4"));

    // Nothing matches: the selection stays and Enter does nothing.
    w.st.search = "qqqq".to_owned();
    w.frame(Vec::new(), nothing);
    assert_eq!(w.st.selected.as_deref(), Some("n4"));
    assert!(w.key(Key::Enter, Modifiers::NONE, nothing).is_empty());
    assert_eq!(w.st.selected.as_deref(), Some("n4"));

    // Without a query every row is a place to go again.
    w.st.search.clear();
    w.frame(Vec::new(), nothing);
    let a = w.key(Key::Enter, Modifiers::NONE, nothing);
    assert!(matches!(&a[..], [TreeAction::Navigate { entry, .. }] if entry == "n4"));
}

#[test]
fn counts_get_thousands_separators() {
    assert_eq!(fmt_count(0), "0");
    assert_eq!(fmt_count(999), "999");
    assert_eq!(fmt_count(1_000), "1,000");
    assert_eq!(fmt_count(22_691), "22,691");
    assert_eq!(fmt_count(1_234_567), "1,234,567");
}

#[test]
fn long_labels_are_clipped_with_an_ellipsis() {
    assert_eq!(clip("short", 24), "short");
    assert_eq!(clip("abcdefghij", 5), "abcd…");
}

#[test]
fn preview_invalid_data_waits_for_a_newer_revision_then_shows_a_note() {
    use std::io;
    fn invalid() -> io::Result<deck_core::tree::EntryDetail> {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session file changed",
        ))
    }
    // The nodes need entry lines, or no request is ever made (stale gateway).
    let mut tree = chain(3);
    for n in &mut tree.nodes {
        let n = Arc::make_mut(n);
        n.line = Some(deck_core::tree::LineSpan { offset: 0, len: 10 });
    }
    let mut w = Window::new(tree);
    w.select_first();
    let tree = Arc::clone(&w.tree);
    let mut input = plain_input();
    input.tree = Some(&tree);
    input.session = Some(std::path::Path::new("/nonexistent.jsonl"));
    // The request goes out (the worker can't answer: no session file).
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(w.st.preview.test_inflight());
    // The file changed under the read: no retry on this revision, nothing cached.
    w.st.preview.test_answer("n0", input.revision, invalid());
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(!w.st.preview.test_cached("n0"));
    assert!(
        !w.st.preview.test_inflight(),
        "no same-frame retry loop after InvalidData"
    );
    // A newer tree revision retries once.
    input.revision += 1;
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(w.st.preview.test_inflight());
    // Failing at the newer revision too: the pane shows a note, never a third request.
    w.st.preview.test_answer("n0", input.revision, invalid());
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(!w.st.preview.test_inflight());
    input.revision += 1;
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(
        !w.st.preview.test_inflight(),
        "twice failed: an inline note, not a third request"
    );
    // A good answer clears the failure record and caches by node id across revisions.
    w.st.preview.test_answer(
        "n0",
        input.revision,
        Ok(deck_core::tree::EntryDetail {
            items: Vec::new(),
            truncated: false,
        }),
    );
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(w.st.preview.test_cached("n0"));
    input.revision += 1;
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(
        w.st.preview.test_cached("n0") && !w.st.preview.test_inflight(),
        "cached entries survive tree revisions without re-reading"
    );
}

#[test]
fn preview_without_entry_lines_never_requests() {
    // A gateway older than the GUI sends trees without `line`: the pane shows the row text
    // with an upgrade note instead of requesting.
    let mut w = Window::new(chain(3));
    w.select_first();
    let tree = Arc::clone(&w.tree);
    let mut input = plain_input();
    input.tree = Some(&tree);
    input.session = Some(std::path::Path::new("/nonexistent.jsonl"));
    preview::poll(&mut w.st, &input, &w.ctx);
    assert!(!w.st.preview.test_inflight());
    assert!(!w.st.preview.test_cached("n0"));
}

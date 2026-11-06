//! The per-frame render context: the snapshot, the ink, the view's persistent local state and the
//! outbox (events for omp, URLs to open, text to copy). Render functions take `&Cx`; the local
//! state and the outbox sit behind `RefCell`s so closures that render twice (measure, then draw)
//! stay `Fn`. Borrows are always short: never hold one across a nested render call.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_tsp::wire::Event;
use egui::{Color32, FontId, Pos2, Rect, TextureHandle};

use super::events::{self, Click};
use super::redirect::Redirect;
use super::select::ComposerSel;
use super::snap::{SNode, Snap};
use super::style::Ink;

/// Everything the frame hands back to the app.
#[derive(Default)]
pub struct Out {
    pub events: Vec<Event>,
    pub open_url: Option<String>,
    pub copy: Option<String>,
    /// Paste the clipboard into the composer (its context menu).
    pub paste: bool,
    /// The user acted on a menu: end the hold the open popup put on the terminal's input.
    pub release: bool,
    /// Blob id of an image to enlarge.
    pub zoom: Option<String>,
    /// The user asked to leave the page omp shows: omp gets an Esc, as if it were typed.
    pub dismiss: bool,
    /// The user clicked a fact that is an omp screen's entry point: OmpDeck opens its own.
    pub redirect: Option<Redirect>,
}

/// A fold flipped locally, until omp's echo (its `collapsed` prop matching the flip) arrives.
/// omp keeps fold state for card roots only; nested nodes (agent rows, checklist phases)
/// never echo, so their flips are terminal-local and hold until the reader flips them back.
/// (An earlier version dropped the flip on any prop change, so a live card's stats ticks
/// snapped it shut a frame after the click.)
#[derive(Debug, Clone, Copy)]
pub struct FoldOverride {
    pub collapsed: bool,
}

/// Look of plain text where the nodes do not say otherwise: the reading face in the transcript,
/// compact mono in tool bodies, the label face in the dock.
#[derive(Clone)]
pub struct Tx {
    pub font: FontId,
    pub color: Color32,
    pub line: Option<f32>,
}

pub struct Cx<'a> {
    pub snap: &'a Snap,
    pub ink: &'a Ink,
    pub now: Instant,
    loc: RefCell<&'a mut Local>,
    out: RefCell<Out>,
    animating: Cell<Option<Duration>>,
    tx: RefCell<Tx>,
    /// The chat has keyboard focus (the caret is live).
    pub focused: bool,
    /// What omp's hello advertised for input: native caret placement.
    pub can_edit: bool,
    /// The app already shows these composer facts for this pane (never for fullscreen pages).
    pub hide_duplicates: bool,
    /// A node of the layer that omp opened and is closing: not drawn (see `redirect`).
    pub hidden: Option<&'a str>,
    /// What OmpDeck knows about this tab's subagents (identity + model per registry id),
    /// for rows omp's TSP leaves bare: a `wait` card's job tiles name a task job but never
    /// carry its model, while omp's own TUI badges it. Borrowed per frame from the app's
    /// already-refreshed agent data: no file or socket reads happen here.
    pub agents: &'a [AgentMeta],
}

/// The identity of one subagent, as the agents dashboard resolved it (transcript plus the
/// live overlay when the control link is up), reduced to what a chat row shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentMeta {
    /// The registry id (the transcript file stem): what TSP tiles and agent rows name.
    pub id: String,
    /// `session_init.agent` ("task", "scout", …).
    pub agent: Option<String>,
    /// `provider/id` (latest model_change, else resolvedModel).
    pub model: Option<String>,
    /// Explicit thinking level, when the agent resolved one.
    pub thinking: Option<String>,
}

impl AgentMeta {
    pub fn of(a: &deck_core::agents::AgentSummary) -> AgentMeta {
        AgentMeta {
            id: a.id.clone(),
            agent: a.agent.clone(),
            model: a.model.clone(),
            thinking: crate::agent_model::agent_thinking(a).map(str::to_owned),
        }
    }
}

/// A toast from the document's ring, kept by `seq` so it shows for its whole `ttl` although omp
/// already deleted its node (and is not shown twice when the ring is read again).
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub seq: u64,
    /// Cleaned for display once, when the toast is taken from the ring.
    pub text: Arc<str>,
    pub sub: Option<Arc<str>>,
    pub at: Instant,
    pub ttl: Duration,
}

/// A toast's laid-out lines: its text never changes while it shows, so they are laid out again
/// only when the display scale or the colours change.
#[derive(Clone)]
pub struct ToastGalleys {
    pub ppp: f32,
    pub colors: [Color32; 2],
    pub text: Arc<egui::Galley>,
    pub sub: Option<Arc<egui::Galley>>,
}

/// What a right click landed on, besides the text under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// A tool or agent card (its node id).
    Card(String),
    /// The composer's editor (its node id).
    Composer(String),
}

/// An open context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtxMenu {
    pub pos: Pos2,
    pub target: Option<Claim>,
}

/// State that lives as long as the tab's chat view (the snapshot sits beside it in `ChatState`
/// so a frame can borrow the snapshot immutably and this mutably).
#[derive(Default)]
pub struct Local {
    pub folds: HashMap<String, FoldOverride>,
    /// The model name each assistant block was first drawn under (by node id): omp tells the
    /// composer's model only, so a block keeps the name it had when it appeared and a later
    /// switch does not relabel replies the earlier model wrote.
    pub bylines: HashMap<String, String>,
    pub heights: HashMap<String, (u64, i32, f32)>,
    /// Estimated heights of items not measured yet, by the same key.
    pub estimates: HashMap<String, (u64, i32, f32)>,
    pub md: super::md::Cache,
    pub ansi: super::ansi::Cache,
    /// Syntax-highlight results of code blocks (self-bounded; owned by `highlight`).
    pub hl: super::highlight::Cache,
    pub textures: HashMap<String, Option<TextureHandle>>,
    pub follow: bool,
    pub jump: bool,
    /// Following, but not at the end (the content grew): scroll back there next frame unless
    /// the reader is scrolling.
    pub chase: bool,
    /// [`Snap::resets`](super::snap::Snap::resets) the transcript last followed the end for.
    pub resets_seen: u64,
    /// A tiny galley laid out every frame: when egui hands out another one, its galley cache or
    /// font atlas was reset and the galleys kept here point into the old atlas.
    pub heads: HashMap<String, (u64, std::sync::Arc<super::tools::HeadText>)>,
    /// Directory shown of a path target at one width: `(generation, width bucket, dir)`.
    pub path_dirs: HashMap<String, (u64, i32, String)>,
    pub probe: Option<std::sync::Arc<egui::Galley>>,
    /// "Copied" feedback by key: a card's action (`<node id>\0<what>`) or a code block.
    pub copied: HashMap<String, Instant>,
    /// The actions of each tool/agent card, by (subtree generation).
    pub acts: HashMap<String, (u64, std::sync::Arc<Vec<super::tools::CardAct>>)>,
    pub zoomed: Option<String>,
    pub blink_epoch: Option<Instant>,
    /// The input method's composition in progress (`Terminal::preedit`), shown at the caret.
    pub preedit: String,
    /// While a tool body is being measured: the runs of equal-height rows its renderers drew
    /// (see [`Cx::note_rows`]).
    pub row_grids: Option<Vec<RowGrid>>,
    /// While a tool body is being measured: the tool's id and whether a block in it is cut by a
    /// clamp it only has while the tool is folded (see [`Cx::note_fold_cut`]).
    pub fold_probe: Option<(String, bool)>,
    /// Whether folding each tool changed what it showed, as measured when last drawn folded
    /// (see `tools::foldable`).
    pub fold_seen: HashMap<String, bool>,
    pub frame: u64,
    /// Screen rect of the composer's caret last frame (anchors the slash menu).
    pub caret: Option<Rect>,
    /// The toasts on screen, by `seq` (see [`Toast`]).
    pub toasts: Vec<Toast>,
    /// The laid-out lines of the toasts in `toasts`, by `seq`.
    pub toast_galleys: HashMap<u64, ToastGalleys>,
    /// The composer's selection (a drag in the editor).
    pub sel: Option<ComposerSel>,
    /// Keys whose press the selection took (their releases never reach omp).
    pub swallowed: Vec<egui::Key>,
    /// The transcript's text selection (see [`super::tsel`]).
    pub tsel: super::tsel::Tsel,
    /// A secondary click this frame; cards and the composer claim it by their rect.
    pub ctx_click: Option<Pos2>,
    pub ctx_claim: Option<Claim>,
    /// The context menu being shown.
    pub menu: Option<CtxMenu>,
    /// A menu entry was clicked: the keyboard focus goes back to the chat (the entry took it).
    pub refocus: bool,
    /// Per-card unified/split override of diffs (key: diff node id; `true` = split).
    pub diff_modes: HashMap<String, bool>,
    /// Live thinking sections the reader expanded past the tail clamp.
    pub thinking_full: HashSet<String>,

    /// Items of each picker, kept across `itemsAdd`/`itemsDel` patches.
    pub pickers: HashMap<String, super::layer::PickerItems>,
    /// Scratch of the transcript's prefix sums (reused between frames).
    pub offsets: Vec<f32>,
}

/// Where the rows of a run of text end.
#[derive(Debug, Clone, PartialEq)]
pub enum RowEnds {
    /// `count` rows of equal height `pitch`.
    Even { pitch: f32, count: usize },
    /// Prefix sums of the row heights, `count + 1` entries from 0 (wrapped rows of any height).
    Prefix(std::sync::Arc<[f32]>),
}

/// A run of text rows, the first starting at `top`.
#[derive(Debug, Clone, PartialEq)]
pub struct RowGrid {
    pub top: f32,
    pub ends: RowEnds,
}

impl RowGrid {
    pub fn count(&self) -> usize {
        match &self.ends {
            RowEnds::Even { count, .. } => *count,
            RowEnds::Prefix(p) => p.len().saturating_sub(1),
        }
    }

    /// Y where row `i` starts (`i == count()`: where the last one ends).
    pub fn edge(&self, i: usize) -> f32 {
        self.top
            + match &self.ends {
                RowEnds::Even { pitch, .. } => i as f32 * pitch,
                RowEnds::Prefix(p) => p
                    .get(i)
                    .copied()
                    .unwrap_or_else(|| p.last().copied().unwrap_or(0.0)),
            }
    }

    pub fn moved(mut self, dy: f32) -> RowGrid {
        self.top += dy;
        self
    }

    /// The rows of several runs, top to bottom, as one run (the gaps between runs belong to the
    /// row that follows them): a body made of a command and its output previews rows of both.
    pub fn merged(mut runs: Vec<RowGrid>) -> Option<RowGrid> {
        runs.sort_by(|a, b| a.top.total_cmp(&b.top));
        let top = runs.first()?.top;
        let rows: usize = runs.iter().map(RowGrid::count).sum();
        let mut ends = Vec::with_capacity(rows + 1);
        ends.push(0.0);
        for g in &runs {
            ends.extend((1..=g.count()).map(|i| g.edge(i) - top));
        }
        Some(RowGrid {
            top,
            ends: RowEnds::Prefix(ends.into()),
        })
    }
}

/// How long a "Copied" feedback is kept; entries older than this are dead weight.
const COPIED_KEEP: Duration = Duration::from_secs(30);

impl Local {
    /// Forgets what belonged to nodes (and blobs) that are gone from `snap`, so a session that
    /// streams for days keeps these maps bounded by what is on screen's document.
    pub fn prune(&mut self, snap: &Snap, now: Instant) {
        let live = |id: &str| snap.get(id).is_some();
        self.heights.retain(|id, _| live(id));
        self.estimates.retain(|id, _| live(id));
        self.heads.retain(|id, _| live(id));
        self.acts.retain(|id, _| live(id));
        self.path_dirs.retain(|id, _| live(id));
        self.pickers.retain(|id, _| live(id));
        self.thinking_full.retain(|id| live(id));
        self.diff_modes.retain(|id, _| live(id));
        // Keys of folds are a node id, optionally followed by `\0` and a suffix.
        self.folds
            .retain(|k, _| live(k.split('\0').next().unwrap_or(k)));
        self.textures.retain(|id, _| snap.blob(id).is_some());
        self.copied
            .retain(|_, t| now.saturating_duration_since(*t) < COPIED_KEEP);
    }
}

impl<'a> Cx<'a> {
    pub fn new(
        snap: &'a Snap,
        ink: &'a Ink,
        loc: &'a mut Local,
        now: Instant,
        focused: bool,
        can_edit: bool,
    ) -> Cx<'a> {
        Cx {
            snap,
            ink,
            now,
            loc: RefCell::new(loc),
            out: RefCell::new(Out::default()),
            animating: Cell::new(None),
            tx: RefCell::new(Tx {
                font: ink.prose(super::style::PROSE),
                color: ink.text(),
                line: Some(super::style::PROSE * super::style::PROSE_LINE),
            }),
            focused,
            can_edit,
            hide_duplicates: false,
            hidden: None,
            agents: &[],
        }
    }

    /// The outbox and when the frame wants to run again (`None`: only on new input/frames).
    pub fn into_out(self) -> (Out, Option<Duration>) {
        (self.out.into_inner(), self.animating.get())
    }

    /// Runs `f` on the local state (keep it short, never render inside).
    pub fn loc<R>(&self, f: impl FnOnce(&mut Local) -> R) -> R {
        f(&mut self.loc.borrow_mut())
    }

    /// A renderer that drew `count` text rows of height `pitch` from `top` reports them, so a
    /// clamped tool body can cut between rows instead of through one. Only taken while a tool
    /// body is being measured; free otherwise.
    pub fn note_rows(&self, top: f32, ends: RowEnds) {
        let grid = RowGrid { top, ends };
        if grid.count() == 0 || !grid.edge(grid.count()).is_finite() {
            return;
        }
        self.loc(|l| {
            if let Some(g) = &mut l.row_grids {
                g.push(grid);
            }
        });
    }

    /// A block whose fold clamp cuts it (`total` lines over its `preview`) reports it, so the
    /// tool it folds with knows folding hides something. Only taken while that tool's body is
    /// being measured; free otherwise.
    pub fn note_fold_cut(&self, n: &SNode) {
        if self.loc(|l| l.fold_probe.is_none()) {
            return;
        }
        let Some(owner) = super::ansi::disclosure(self, n) else {
            return;
        };
        self.loc(|l| {
            if let Some((id, cut)) = &mut l.fold_probe
                && *id == owner.id
            {
                *cut = true;
            }
        });
    }

    /// Something on screen moves continuously: the frame repaints at once.
    pub fn animate(&self) {
        self.animate_in(Duration::from_millis(16));
    }

    /// Something changes by itself after `after` (a blink, a clock tick).
    pub fn animate_in(&self, after: Duration) {
        self.animating
            .set(Some(self.animating.get().map_or(after, |d| d.min(after))));
    }

    /// The current plain-text look.
    pub fn tx(&self) -> Tx {
        self.tx.borrow().clone()
    }

    /// Runs `f` with `tx` as the plain-text look.
    pub fn with_tx<R>(&self, tx: Tx, f: impl FnOnce() -> R) -> R {
        let old = self.tx.replace(tx);
        let r = f();
        *self.tx.borrow_mut() = old;
        r
    }

    pub fn send(&self, e: Event) {
        self.out.borrow_mut().events.push(e);
    }

    pub fn open(&self, url: String) {
        self.out.borrow_mut().open_url = Some(url);
    }

    pub fn copy(&self, text: String) {
        self.out.borrow_mut().copy = Some(text);
    }

    pub fn paste(&self) {
        self.out.borrow_mut().paste = true;
    }

    /// A menu entry was clicked: its events must reach omp although the popup still holds input.
    pub fn release(&self) {
        self.out.borrow_mut().release = true;
    }

    /// Leave the page omp shows (its fullscreen overlay): the view sends omp an Esc key.
    pub fn dismiss(&self) {
        self.out.borrow_mut().dismiss = true;
    }

    pub fn zoom(&self, blob: String) {
        self.out.borrow_mut().zoom = Some(blob);
    }

    pub fn sf(&self) -> &'a str {
        self.snap.sf.as_str()
    }

    /// `n`'s fold state: a local flip omp has not echoed yet, else its `collapsed` prop.
    /// A flip survives unrelated prop updates (a live card's stats ticks); it retires once
    /// omp's prop matches it — or when the reader flips the node back to omp's state.
    pub fn collapsed(&self, n: &SNode) -> bool {
        let flip = self.loc(|l| match l.folds.get(&n.id).copied() {
            Some(o) if o.collapsed == n.flag("collapsed") => {
                l.folds.remove(&n.id);
                None
            }
            o => o,
        });
        flip.map_or_else(|| n.flag("collapsed"), |o| o.collapsed)
    }

    /// Folds/unfolds `n`: flips it locally and tells omp.
    pub fn toggle(&self, n: &SNode) {
        let collapsed = !self.collapsed(n);
        self.loc(|l| {
            if collapsed == n.flag("collapsed") {
                // Back to omp's state: no override left to echo.
                l.folds.remove(&n.id);
            } else {
                l.folds.insert(n.id.clone(), FoldOverride { collapsed });
            }
        });
        self.send(events::toggle(self.sf(), &n.id, n.str("key"), collapsed));
    }

    /// Applies the click action `name` of `n` (what its `actions` declare).
    pub fn click(&self, n: &SNode, name: &str) {
        // The facts that open omp's model picker and usage dashboard open OmpDeck's own.
        let redirect = match name {
            "status.model" => Some(Redirect::ModelPicker),
            "status.cost" => Some(Redirect::Usage { reset: false }),
            "agents.open" => Some(Redirect::AgentHub { activity: false }),
            _ => None,
        };
        if redirect.is_some() {
            self.out.borrow_mut().redirect = redirect;
            return;
        }
        match events::resolve_click(self.snap, n, name, self.collapsed(n)) {
            Click::Toggle(_) => self.toggle(n),
            Click::Copy => self.copy(super::text::node_plain(n)),
            Click::Open => {
                if let Some(u) = n.str("href").or_else(|| n.str("text")) {
                    self.open(u.to_owned());
                }
            }
            Click::Zoom => {
                if let Some(b) = n.str("blob") {
                    self.zoom(b.to_owned());
                }
            }
            Click::Send(e) => self.send(e),
        }
    }

    /// A click on a list/tab row: its own `actions.click` if it has one, else `select` (or
    /// `activate` for a double click or a click on the selected row) addressed to its list.
    pub fn click_item(&self, n: &SNode, list: Option<&SNode>, activate: bool) {
        if let Some(act) = n.click() {
            return self.click(n, act);
        }
        let list = list.or_else(|| events::list_of(self.snap, n));
        let target = list.map_or(n.id.as_str(), |l| l.id.as_str());
        self.send(if activate {
            events::activate(self.sf(), target, &n.id)
        } else {
            events::select(self.sf(), target, &n.id)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_view::snap::tests::{fixture_mirror, sync_doc};

    #[test]
    fn a_fold_survives_unrelated_prop_updates_until_omp_echoes_it() {
        use crate::chat_view::snap::tests::feed_frame;
        use serde_json::json;
        let mut m = fixture_mirror(usize::MAX);
        // A collapsed task card like omp's (see the captured `Task · 2 agents` card).
        feed_frame(
            &mut m,
            &json!([[
                "add",
                "t",
                "main",
                serde_json::Value::Null,
                {
                    "id": "t",
                    "k": "tool",
                    "p": {
                        "role": "omp.tool.task",
                        "name": "task",
                        "title": "Task",
                        "target": "2 agents",
                        "targetKind": "text",
                        "status": "running",
                        "frame": "card",
                        "collapsible": true,
                        "collapsed": true,
                        "preview": "none",
                    },
                },
            ]]),
        );
        let mut s = Snap::default();
        sync_doc(&mut s, &m);
        let tool = "t".to_owned();
        let gen0 = s.get(&tool).map(|n| n.gen_).unwrap_or(0);
        let mut loc = Local::default();
        let ink = crate::chat_view::style::test_ink();
        // Expand: the flip shows at once, ahead of omp's echo.
        {
            let cx = Cx::new(&s, &ink, &mut loc, Instant::now(), true, true);
            let n = cx.snap.get(&tool).expect("tool");
            assert!(cx.collapsed(n), "omp's fixture card starts collapsed");
            cx.toggle(n);
            assert!(!cx.collapsed(n), "the flip shows at once");
        }
        // omp redraws the card for a live update (a stats tick) without touching
        // `collapsed`: the generation moves but the fold holds.
        feed_frame(&mut m, &json!([["set", tool, {"stats": {"age": 4321}}]]));
        assert!(sync_doc(&mut s, &m), "the update syncs");
        assert_ne!(
            s.get(&tool).map(|n| n.gen_),
            Some(gen0),
            "the update moved gen_"
        );
        {
            let cx = Cx::new(&s, &ink, &mut loc, Instant::now(), true, true);
            let n = cx.snap.get(&tool).expect("tool");
            assert!(
                !cx.collapsed(n),
                "a live update must not snap the card shut"
            );
        }
        // omp's echo (the `collapsed` prop now matches the flip) retires the override.
        feed_frame(&mut m, &json!([["set", tool, {"collapsed": false}]]));
        assert!(sync_doc(&mut s, &m), "the echo syncs");
        {
            let cx = Cx::new(&s, &ink, &mut loc, Instant::now(), true, true);
            let n = cx.snap.get(&tool).expect("tool");
            assert!(!cx.collapsed(n), "the echo keeps the card open");
            assert!(
                !loc.folds.contains_key(&tool),
                "the echo retires the override"
            );
        }
    }

    #[test]
    fn pruning_forgets_what_belongs_to_nodes_that_are_gone() {
        let m = fixture_mirror(usize::MAX);
        let mut snap = Snap::default();
        sync_doc(&mut snap, &m);
        let live = "main".to_owned();
        assert!(snap.get(&live).is_some());
        let now = Instant::now();
        let mut l = Local::default();
        for id in [live.as_str(), "gone"] {
            l.heights.insert(id.to_owned(), (1, 600, 10.0));
            l.estimates.insert(id.to_owned(), (1, 600, 10.0));
            l.path_dirs.insert(id.to_owned(), (1, 600, String::new()));
            l.copied.insert(id.to_owned(), now);
        }
        let fold = FoldOverride { collapsed: true };
        l.folds.insert(format!("{live}\0more"), fold);
        l.folds.insert("gone\0more".to_owned(), fold);
        l.folds.insert("gone".to_owned(), fold);
        l.copied.insert(
            "stale".to_owned(),
            now - COPIED_KEEP - Duration::from_secs(1),
        );
        l.prune(&snap, now);
        assert_eq!(l.heights.keys().collect::<Vec<_>>(), [&live]);
        assert_eq!(l.estimates.keys().collect::<Vec<_>>(), [&live]);
        assert_eq!(l.path_dirs.keys().collect::<Vec<_>>(), [&live]);
        assert_eq!(l.folds.len(), 1, "{:?}", l.folds.keys());
        assert!(l.folds.contains_key(&format!("{live}\0more")));
        assert!(l.copied.contains_key(&live) && !l.copied.contains_key("stale"));
    }
}

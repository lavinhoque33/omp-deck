//! Keyboard map of the tree window: omp's chords (arrows, pages, Alt message jumps,
//! `[`/`]` branch jumps, Enter go, Shift+Enter summarize, F fork, L/Shift+L label, `/` search,
//! Alt filter letters, Ctrl+O cycling, Ctrl+C close).

use egui::{Key, Modifiers};

use crate::tree_layout::{self, Filter, Layout};
use crate::tree_view::{TreeState, step_row};
use deck_core::tree::SessionTree;

/// One selection move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// Up / down by `n` rows.
    Step(isize),
    /// A page up (-1) or down (+1).
    Page(isize),
    Home,
    End,
    /// Previous / next user or assistant message (Alt+Up / Alt+Down).
    Turn {
        forward: bool,
    },
    /// Previous / next branch (`[` / `]`).
    Branch {
        forward: bool,
    },
}

/// The row `mv` selects from row `cur`; `page` is the number of rows the list shows.
pub fn move_target(
    tree: &SessionTree,
    layout: &Layout,
    cur: Option<usize>,
    mv: Move,
    page: usize,
) -> Option<usize> {
    let len = layout.rows.len();
    match mv {
        Move::Step(n) => step_row(cur, n, len),
        Move::Page(n) => step_row(cur, n * page.max(1) as isize, len),
        Move::Home => step_row(Some(0), 0, len),
        Move::End => len.checked_sub(1),
        Move::Turn { forward } => tree_layout::turn_jump(tree, layout, cur?, forward),
        Move::Branch { forward } => tree_layout::branch_jump(tree, layout, cur?, forward),
    }
}

/// The per-row match flags of an active query that matches something (`None` otherwise: no
/// query, or nothing matches and the list is left as it is).
pub fn search_rows(st: &TreeState) -> Option<&[bool]> {
    st.hits
        .as_ref()
        .map(|h| &h.rows[..])
        .filter(|rows| rows.contains(&true))
}

/// Whether a query is active and row `r` is not among its matches.
pub fn excluded_by_search(st: &TreeState, r: usize) -> bool {
    st.hits
        .as_ref()
        .is_some_and(|h| !h.rows.is_empty() && !h.rows.get(r).copied().unwrap_or(false))
}

/// The match to rest on from row `from`: itself, else the first one after it, else the first.
pub fn nearest_match(rows: &[bool], from: Option<usize>) -> Option<usize> {
    let start = from.unwrap_or(0).min(rows.len());
    rows[start..]
        .iter()
        .position(|m| *m)
        .map(|i| start + i)
        .or_else(|| rows.iter().position(|m| *m))
}

/// `mv` among the matching rows only (they are the list while a query is typed); the moves that
/// do not walk the list (message and branch jumps) are left to [`move_target`].
pub fn match_target(rows: &[bool], cur: Option<usize>, mv: Move, page: usize) -> Option<usize> {
    let matches: Vec<usize> = (0..rows.len()).filter(|&r| rows[r]).collect();
    let last = matches.len().checked_sub(1)?;
    let steps = match mv {
        Move::Step(n) => n,
        Move::Page(n) => n * page.max(1) as isize,
        Move::Home => return matches.first().copied(),
        Move::End => return matches.last().copied(),
        Move::Turn { .. } | Move::Branch { .. } => return None,
    };
    // Matches before `cur`; a match at `cur` itself is where the walk starts.
    let Some(cur) = cur else {
        return Some(matches[if steps >= 0 { 0 } else { last }]);
    };
    let before = matches.partition_point(|&m| m < cur);
    let on_match = matches.get(before) == Some(&cur);
    // From a non-match the first step down lands on the next match, the first step up on the
    // previous one.
    let at = if on_match {
        before as isize + steps
    } else if steps > 0 {
        before as isize + steps - 1
    } else {
        before as isize + steps
    };
    Some(matches[at.clamp(0, last as isize) as usize])
}

/// The label key is a command only while no search is typed and the box is not being typed in.
pub fn label_ok(searching: bool, search: &str) -> bool {
    !searching && search.is_empty()
}

/// What the keyboard asked for this frame.
#[derive(Default)]
pub struct Keys {
    pub esc: bool,
    pub enter: bool,
    /// Shift+Enter: go there with a summary of the branch left (omp pre-answers "Summarize").
    pub summarize: bool,
    pub fork: bool,
    /// `L` / Shift+L: edit the selected entry's label.
    pub label: bool,
    pub slash: bool,
    /// Ctrl+C: close the tree (omp: `ctrl+c` closes).
    pub close: bool,
    /// Ctrl+C while something is selected would copy — reserved: preview Copy only.
    pub copy: bool,
    pub mv: Option<Move>,
    /// Alt+C / D / T / U / L / A.
    pub filter: Option<Filter>,
    /// Ctrl+O (+1) / Ctrl+Shift+O (-1).
    pub cycle: Option<isize>,
}

/// Consume the tree's keys. omp's chords go first because egui ignores extra Shift / Alt when it
/// matches a key: a plain `ArrowUp` pattern would also take Alt+Up, Ctrl+O would take Ctrl+Shift+O.
/// Letters and ← → belong to the search box while it has focus.
pub fn read(i: &mut egui::InputState, searching: bool, menu_open: bool, label_ok: bool) -> Keys {
    let n = Modifiers::NONE;
    let mut k = Keys::default();
    let mut alt = false;
    for (letter, key) in [
        (Filter::Conversation, Key::C),
        (Filter::Default, Key::D),
        (Filter::NoTools, Key::T),
        (Filter::UserOnly, Key::U),
        (Filter::Labeled, Key::L),
        (Filter::All, Key::A),
    ] {
        if !menu_open && i.consume_key(Modifiers::ALT, key) {
            k.filter = Some(letter);
            alt = true;
        }
    }
    if !menu_open {
        if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::O) {
            k.cycle = Some(-1);
        } else if i.consume_key(Modifiers::COMMAND, Key::O) {
            k.cycle = Some(1);
        }
        for (key, forward) in [(Key::ArrowUp, false), (Key::ArrowDown, true)] {
            if i.consume_key(Modifiers::ALT, key) {
                k.mv = Some(Move::Turn { forward });
                alt = true;
            }
        }
    }
    if alt {
        // Linux delivers Alt+D as the text "d" as well; the search box must not type it.
        i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
    }
    k.esc = !menu_open && i.consume_key(n, Key::Escape);
    // Ctrl+C closes (unless a popup/menu is open); plain C keeps its filter meaning via Alt only.
    k.close = !menu_open && !searching && i.consume_key(Modifiers::COMMAND, Key::C);
    // Shift+Enter first: a plain Enter pattern would take it too.
    k.summarize = !menu_open && i.consume_key(Modifiers::SHIFT, Key::Enter);
    k.enter = !menu_open && (k.summarize || i.consume_key(n, Key::Enter));
    k.fork = !searching && !menu_open && i.consume_key(n, Key::F);
    // Shift+L first (plain L would take it too: egui ignores the extra Shift).
    let shift_l = label_ok && !menu_open && i.consume_key(Modifiers::SHIFT, Key::L);
    k.label = shift_l || (label_ok && !menu_open && i.consume_key(n, Key::L));
    k.slash = !searching && !menu_open && i.consume_key(n, Key::Slash)
        || i.consume_key(Modifiers::COMMAND, Key::F);
    let mut plain = |key: Key, mv: Move| {
        if i.consume_key(n, key) && k.mv.is_none() {
            k.mv = Some(mv);
        }
    };
    plain(Key::ArrowUp, Move::Step(-1));
    plain(Key::ArrowDown, Move::Step(1));
    plain(Key::PageUp, Move::Page(-1));
    plain(Key::PageDown, Move::Page(1));
    if !searching {
        plain(Key::ArrowLeft, Move::Page(-1));
        plain(Key::ArrowRight, Move::Page(1));
        plain(Key::Home, Move::Home);
        plain(Key::End, Move::End);
        if !menu_open {
            plain(Key::OpenBracket, Move::Branch { forward: false });
            plain(Key::CloseBracket, Move::Branch { forward: true });
        }
    }
    k
}

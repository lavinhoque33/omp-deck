//! Split-view model: which tab (by uid) sits in which pane slot, which pane has focus, where the
//! dividers are, and the geometry of the panes. Pure state, no egui widgets, so every rule is unit
//! tested here; `app.rs` keeps `App.active` equal to the focused pane's tab.
//!
//! Slots are numbered row-major per layout: Two columns `0|1`, two rows `0/1`, three columns
//! `0|1|2`, grid `0 1 / 2 3`. Tabs are referenced by uid, so tabs opening and closing elsewhere
//! in the strip never shift a slot.

use std::collections::HashMap;

use deck_core::config::{SplitLayout, SplitState};
use egui::{Pos2, Rect, pos2, vec2};

/// Most panes a layout holds.
pub const MAX_PANES: usize = 4;
/// Width of the gap between panes, which is also the divider's grab area.
pub const GAP: f32 = 6.0;
/// Smallest share of the area a pane may be dragged down to.
const MIN_SHARE: f32 = 0.15;

fn slots_hold(slots: &[Option<u64>], uid: u64) -> bool {
    slots.contains(&Some(uid))
}

/// Where an "open in split" puts the new pane relative to the focused one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Right,
    Down,
}

/// A focus move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Number of pane slots of `layout`.
pub fn slot_count(layout: SplitLayout) -> usize {
    match layout {
        SplitLayout::Single => 1,
        SplitLayout::Columns2 | SplitLayout::Rows2 => 2,
        SplitLayout::Columns3 => 3,
        SplitLayout::Grid4 => 4,
    }
}

/// Divider positions of a fresh layout, as fractions of the area: columns/rows by their cut
/// positions in ascending order; the grid as `[x of the column cut, y of the row cut]`.
pub fn default_ratios(layout: SplitLayout) -> Vec<f32> {
    match layout {
        SplitLayout::Single => Vec::new(),
        SplitLayout::Columns2 | SplitLayout::Rows2 => vec![0.5],
        SplitLayout::Columns3 => vec![1.0 / 3.0, 2.0 / 3.0],
        SplitLayout::Grid4 => vec![0.5, 0.5],
    }
}

/// (column, row) of a slot.
fn cell(layout: SplitLayout, slot: usize) -> (usize, usize) {
    match layout {
        SplitLayout::Single => (0, 0),
        SplitLayout::Columns2 | SplitLayout::Columns3 => (slot, 0),
        SplitLayout::Rows2 => (0, slot),
        SplitLayout::Grid4 => (slot % 2, slot / 2),
    }
}

fn valid_ratios(layout: SplitLayout, ratios: &[f32]) -> bool {
    if ratios.len() != default_ratios(layout).len() || ratios.iter().any(|r| !r.is_finite()) {
        return false;
    }
    const EPS: f32 = 1e-4;
    let in_range = |r: f32| (MIN_SHARE - EPS..=1.0 - MIN_SHARE + EPS).contains(&r);
    match layout {
        SplitLayout::Columns3 => {
            in_range(ratios[0]) && in_range(ratios[1]) && ratios[1] - ratios[0] >= MIN_SHARE - EPS
        }
        _ => ratios.iter().all(|r| in_range(*r)),
    }
}

/// The pane arrangement of the window.
#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    layout: SplitLayout,
    slots: Vec<Option<u64>>,
    focused: usize,
    ratios: Vec<f32>,
    /// Tick of each shown tab's last focus: the replacement victim at four panes is the oldest.
    stamps: HashMap<u64, u64>,
    tick: u64,
    /// Counter of reservations, which live in `slots` as `TICKET_BASE + n` so they move with
    /// their pane when the layout changes.
    tickets: u64,
}

/// Reservations are numbered from here; tab uids never get this high.
const TICKET_BASE: u64 = 1 << 63;

fn is_ticket(uid: u64) -> bool {
    uid >= TICKET_BASE
}

impl Default for Split {
    fn default() -> Self {
        Split {
            layout: SplitLayout::Single,
            slots: vec![None],
            focused: 0,
            ratios: Vec::new(),
            stamps: HashMap::new(),
            tick: 0,
            tickets: 0,
        }
    }
}

impl Split {
    pub fn layout(&self) -> SplitLayout {
        self.layout
    }

    /// More than one pane.
    pub fn is_split(&self) -> bool {
        self.layout != SplitLayout::Single
    }

    /// The pane contents: a tab uid, `None` (empty), or a reservation (see
    /// [`Split::is_reserved`]; it never equals a tab's uid).
    pub fn slots(&self) -> &[Option<u64>] {
        &self.slots
    }

    pub fn focused(&self) -> usize {
        self.focused
    }

    pub fn focused_uid(&self) -> Option<u64> {
        self.slots
            .get(self.focused)
            .copied()
            .flatten()
            .filter(|u| !is_ticket(*u))
    }

    pub fn ratios(&self) -> &[f32] {
        &self.ratios
    }

    /// The slot showing `uid`.
    pub fn slot_of(&self, uid: u64) -> Option<usize> {
        self.slots.iter().position(|s| *s == Some(uid))
    }

    /// Every tab shown in a pane.
    pub fn uids(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots
            .iter()
            .copied()
            .flatten()
            .filter(|u| !is_ticket(*u))
    }

    fn touch(&mut self, slot: usize) {
        self.tick += 1;
        if let Some(uid) = self.slots[slot].filter(|u| !is_ticket(*u)) {
            self.stamps.insert(uid, self.tick);
        }
    }

    /// Focus `slot` (an empty slot can hold focus). False when it does not exist.
    pub fn focus_slot(&mut self, slot: usize) -> bool {
        if slot >= self.slots.len() {
            return false;
        }
        if slot != self.focused {
            self.focused = slot;
        }
        self.touch(slot);
        true
    }

    /// Focus the pane showing `uid`.
    pub fn focus_uid(&mut self, uid: u64) -> bool {
        self.slot_of(uid).is_some_and(|s| self.focus_slot(s))
    }

    /// A tab became the active one: its pane takes focus when it already has one, otherwise it
    /// replaces the focused pane's tab. Keeps "a tab is in at most one pane".
    pub fn show(&mut self, uid: u64) {
        if self.focus_uid(uid) {
            return;
        }
        if let Some(old) = self.slots[self.focused].replace(uid) {
            self.stamps.remove(&old);
        }
        self.touch(self.focused);
    }

    /// Open `uid` in a new pane beside the focused one, which the new pane then holds the focus
    /// of. A tab that is already shown just takes focus. At four panes the new tab replaces an
    /// empty pane, else the least recently focused one other than the focused pane.
    ///
    /// `None` opens an empty pane that waits for a tab; focus stays on the pane that had it (an
    /// empty pane has no terminal to type into, so it must not take the keyboard).
    pub fn open_split(&mut self, uid: Option<u64>, side: Side) {
        if let Some(u) = uid
            && self.focus_uid(u)
        {
            return;
        }
        self.open_pane(uid, side, uid.is_none());
    }

    /// Open an empty pane reserved for a tab that is still being launched. Focus stays on the
    /// pane that had it until [`Split::fill_reserved`] puts the tab in exactly this pane.
    pub fn reserve_split(&mut self, side: Side) -> u64 {
        let ticket = self.new_ticket();
        self.open_pane(Some(ticket), side, true);
        ticket
    }

    fn new_ticket(&mut self) -> u64 {
        self.tickets += 1;
        TICKET_BASE + self.tickets
    }

    fn open_pane(&mut self, uid: Option<u64>, side: Side, keep_focus: bool) {
        let keep = self.focused_uid();
        let f = self.focused;
        let s = self.slots.clone();
        let (layout, slots, at) = match (self.layout, side) {
            (SplitLayout::Single, Side::Right) => (SplitLayout::Columns2, vec![s[0], uid], 1),
            (SplitLayout::Single, Side::Down) => (SplitLayout::Rows2, vec![s[0], uid], 1),
            (SplitLayout::Columns2, Side::Right) => {
                let mut v = s;
                v.insert(f + 1, uid);
                (SplitLayout::Columns3, v, f + 1)
            }
            // The new pane goes under the focused column.
            (SplitLayout::Columns2, Side::Down) => {
                let mut v = vec![s[0], s[1], None, None];
                v[2 + f] = uid;
                (SplitLayout::Grid4, v, 2 + f)
            }
            // No three-row layout: the new pane becomes a second column next to the focused row.
            (SplitLayout::Rows2, _) => {
                let mut v = vec![s[0], None, s[1], None];
                v[1 + 2 * f] = uid;
                (SplitLayout::Grid4, v, 1 + 2 * f)
            }
            (SplitLayout::Columns3, _) => (SplitLayout::Grid4, vec![s[0], s[1], s[2], uid], 3),
            (SplitLayout::Grid4, _) => {
                let victim = self.replacement_victim();
                if let Some(old) = std::mem::replace(&mut self.slots[victim], uid) {
                    self.stamps.remove(&old);
                }
                self.focus_slot(victim);
                self.keep_focus_on(keep, keep_focus);
                return;
            }
        };
        self.layout = layout;
        self.slots = slots;
        self.ratios = default_ratios(layout);
        self.stamps.retain(|u, _| slots_hold(&self.slots, *u));
        self.focused = at;
        self.touch(at);
        self.keep_focus_on(keep, keep_focus);
    }

    fn keep_focus_on(&mut self, keep: Option<u64>, wanted: bool) {
        if wanted && let Some(k) = keep {
            self.focus_uid(k);
        }
    }

    /// The tab launched for `ticket` exists: it takes exactly the pane reserved for it. When
    /// that pane is gone (the user closed it) the tab takes the first empty pane instead, and
    /// when none is empty it is left to the caller to show it like any opened tab. True when the
    /// tab was placed.
    pub fn fill_reserved(&mut self, ticket: u64, uid: u64) -> bool {
        if self.slot_of(uid).is_some() {
            self.release(ticket);
            return false;
        }
        match self.slots.iter().position(|s| *s == Some(ticket)) {
            Some(slot) => {
                self.slots[slot] = Some(uid);
                self.touch(slot);
                true
            }
            None => self.fill_vacated(uid),
        }
    }

    /// The tab behind `ticket` will never come (its launch failed or was dropped): the reserved
    /// pane goes away.
    pub fn release(&mut self, ticket: u64) {
        if let Some(slot) = self.slots.iter().position(|s| *s == Some(ticket)) {
            if self.is_split() {
                self.close_slot(slot);
            } else {
                self.slots[slot] = None;
            }
        }
    }

    /// The tab in `uid`'s pane is going away and a launch will replace it (a restart, a move):
    /// the pane is reserved for exactly that launch. Focus moves to a pane that shows a tab
    /// meanwhile. `None` when `uid` has no pane.
    pub fn reserve_slot_of(&mut self, uid: u64) -> Option<u64> {
        let slot = self.slot_of(uid)?;
        let ticket = self.new_ticket();
        self.slots[slot] = Some(ticket);
        self.stamps.remove(&uid);
        if self.focused == slot
            && let Some(other) = (0..self.slots.len())
                .filter(|i| self.slots[*i].is_some_and(|u| !is_ticket(u)))
                .min_by_key(|i| i.abs_diff(slot))
        {
            self.focus_slot(other);
        }
        Some(ticket)
    }

    /// Tickets of the panes still waiting for a launch.
    pub fn tickets(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots
            .iter()
            .copied()
            .flatten()
            .filter(|u| is_ticket(*u))
    }

    /// Whether pane `slot` waits for a launch.
    pub fn is_reserved(&self, slot: usize) -> bool {
        self.slots
            .get(slot)
            .copied()
            .flatten()
            .is_some_and(is_ticket)
    }

    /// An empty pane, else the oldest non-focused tab pane; a pane waiting for a launch is only
    /// taken when nothing else is left.
    fn replacement_victim(&self) -> usize {
        if let Some(i) = self
            .slots
            .iter()
            .enumerate()
            .position(|(i, s)| s.is_none() && i != self.focused)
        {
            return i;
        }
        (0..self.slots.len())
            .filter(|i| *i != self.focused)
            .min_by_key(|i| match self.slots[*i] {
                Some(u) if is_ticket(u) => (true, 0),
                Some(u) => (false, self.stamps.get(&u).copied().unwrap_or(0)),
                None => (false, 0),
            })
            .unwrap_or(self.focused)
    }

    /// Switch to the 2×2 grid, keeping the current panes in order and filling empty slots from
    /// `spare` (tabs not shown yet, in the order they should appear).
    pub fn make_grid(&mut self, spare: &[u64]) {
        let mut slots = self.slots.clone();
        slots.resize(MAX_PANES, None);
        let mut spare = spare.iter().copied().filter(|u| self.slot_of(*u).is_none());
        for s in slots.iter_mut().filter(|s| s.is_none()) {
            *s = spare.next();
        }
        self.layout = SplitLayout::Grid4;
        self.ratios = default_ratios(SplitLayout::Grid4);
        self.slots = slots;
    }

    /// Back to one pane showing the focused pane's tab.
    pub fn unsplit(&mut self) {
        let keep = self.focused_uid();
        *self = Split::default();
        self.slots[0] = keep;
    }

    /// Remove pane `slot`: the layout shrinks (grid → three columns → two columns → single), the
    /// panes that remain keep their order. Focus stays on the same tab, or moves to the pane that
    /// took the closed one's place. Closing the only pane empties it.
    pub fn close_slot(&mut self, slot: usize) {
        if slot >= self.slots.len() {
            return;
        }
        if !self.is_split() {
            self.slots[0] = None;
            return;
        }
        if let Some(uid) = self.slots.remove(slot) {
            self.stamps.remove(&uid);
        }
        let was_focused = slot == self.focused;
        self.layout = match self.slots.len() {
            1 => SplitLayout::Single,
            2 => SplitLayout::Columns2,
            _ => SplitLayout::Columns3,
        };
        self.ratios = default_ratios(self.layout);
        self.focused = if was_focused {
            // The neighbour that slid into the gap, preferring one that shows a tab.
            let near = slot.min(self.slots.len() - 1);
            (0..self.slots.len())
                .min_by_key(|i| (self.slots[*i].is_none(), i.abs_diff(near)))
                .unwrap_or(0)
        } else if slot < self.focused {
            self.focused - 1
        } else {
            self.focused
        };
        self.touch(self.focused);
    }

    /// The tab was closed for good: its pane goes with it, and so do panes that stand empty
    /// (they only wait for a tab). True when it had a pane.
    pub fn remove_uid(&mut self, uid: u64) -> bool {
        let Some(slot) = self.slot_of(uid) else {
            return false;
        };
        self.close_slot(slot);
        while self.is_split()
            && let Some(empty) = self.slots.iter().position(Option::is_none)
        {
            self.close_slot(empty);
        }
        true
    }

    /// The tab is going away but a replacement follows (a restart): keep its pane, empty.
    pub fn vacate(&mut self, uid: u64) {
        if let Some(slot) = self.slot_of(uid) {
            self.slots[slot] = None;
        }
    }

    /// A tab opened while a pane stands empty: it takes the first empty pane. True when placed.
    pub fn fill_vacated(&mut self, uid: u64) -> bool {
        if !self.is_split() || self.slot_of(uid).is_some() {
            return false;
        }
        match self.slots.iter().position(Option::is_none) {
            Some(slot) => {
                self.slots[slot] = Some(uid);
                true
            }
            None => false,
        }
    }

    /// Drop every pane whose tab `live` rejects (a safety net for tabs that vanish unannounced).
    pub fn retain(&mut self, live: impl Fn(u64) -> bool) {
        for uid in self.uids().filter(|u| !live(*u)).collect::<Vec<_>>() {
            if self.is_split() {
                self.remove_uid(uid);
            } else {
                self.vacate(uid);
            }
        }
    }

    /// The focus moves to the neighbouring pane in `dir`. False at the edge.
    pub fn move_focus(&mut self, dir: Dir) -> bool {
        let (c, r) = cell(self.layout, self.focused);
        let target = match dir {
            Dir::Left => c.checked_sub(1).map(|c| (c, r)),
            Dir::Right => Some((c + 1, r)),
            Dir::Up => r.checked_sub(1).map(|r| (c, r)),
            Dir::Down => Some((c, r + 1)),
        };
        let Some(target) = target else {
            return false;
        };
        match (0..self.slots.len()).find(|s| cell(self.layout, *s) == target) {
            Some(slot) => self.focus_slot(slot),
            None => false,
        }
    }

    /// Focus the next (`+1`) or previous (`-1`) pane, wrapping around.
    pub fn cycle_focus(&mut self, delta: isize) {
        let n = self.slots.len().cast_signed();
        let next = (self.focused.cast_signed() + delta).rem_euclid(n);
        self.focus_slot(next.cast_unsigned());
    }

    /// Drag divider `index` to `value` (a fraction of the area), keeping every pane at least
    /// [`MIN_SHARE`] wide.
    pub fn set_divider(&mut self, index: usize, value: f32) {
        if index >= self.ratios.len() || !value.is_finite() {
            return;
        }
        let (lo, hi) = match (self.layout, index) {
            (SplitLayout::Columns3, 0) => (MIN_SHARE, self.ratios[1] - MIN_SHARE),
            (SplitLayout::Columns3, _) => (self.ratios[0] + MIN_SHARE, 1.0 - MIN_SHARE),
            _ => (MIN_SHARE, 1.0 - MIN_SHARE),
        };
        self.ratios[index] = value.clamp(lo, hi.max(lo));
    }

    /// The layout to persist (`None` for a single pane). `persisted` lists the uids of the tabs
    /// written to `open_tabs`, in that order; panes whose tab is not persisted become empty.
    pub fn to_state(&self, persisted: &[u64]) -> Option<SplitState> {
        if !self.is_split() {
            return None;
        }
        Some(SplitState {
            layout: self.layout,
            panes: self
                .slots
                .iter()
                .map(|s| s.and_then(|u| persisted.iter().position(|p| *p == u)))
                .collect(),
            focused: self.focused,
            ratios: self.ratios.clone(),
        })
    }

    /// Rebuild a persisted layout. `resolve` maps an `open_tabs` index to the restored tab's uid;
    /// indices that resolve to nothing, and repeated tabs, are dropped and their panes collapse.
    /// Invalid ratios fall back to the defaults.
    pub fn from_state(state: &SplitState, resolve: impl Fn(usize) -> Option<u64>) -> Split {
        let n = slot_count(state.layout);
        let mut seen = Vec::new();
        let slots: Vec<Option<u64>> = (0..n)
            .map(|i| {
                let uid = state.panes.get(i).copied().flatten().and_then(&resolve)?;
                if seen.contains(&uid) {
                    return None;
                }
                seen.push(uid);
                Some(uid)
            })
            .collect();
        let ratios = if valid_ratios(state.layout, &state.ratios) {
            state.ratios.clone()
        } else {
            default_ratios(state.layout)
        };
        let intact = slots.iter().all(Option::is_some);
        let mut split = Split {
            layout: state.layout,
            stamps: HashMap::new(),
            slots,
            focused: state.focused.min(n - 1),
            ratios,
            tick: 0,
            tickets: 0,
        };
        if !intact {
            // Collapse the panes that lost their tab, back to front so indices stay valid.
            let focus = split.focused_uid();
            for slot in (0..n).rev() {
                if split.slots[slot].is_none() && split.is_split() {
                    split.close_slot(slot);
                }
            }
            if let Some(uid) = focus {
                split.focus_uid(uid);
            }
        }
        split
    }
}

/// A draggable divider between panes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Divider {
    /// Index into [`Split::ratios`].
    pub index: usize,
    /// A vertical bar (it moves left/right) rather than a horizontal one.
    pub vertical: bool,
    /// The grab area, [`GAP`] wide.
    pub rect: Rect,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Pane rectangles (slot order) tiling `area`, [`GAP`] apart.
pub fn pane_rects(layout: SplitLayout, area: Rect, ratios: &[f32]) -> Vec<Rect> {
    let xs = |cuts: &[f32]| -> Vec<f32> {
        let mut v = vec![area.left()];
        v.extend(cuts.iter().map(|c| lerp(area.left(), area.right(), *c)));
        v.push(area.right());
        v
    };
    let ys = |cuts: &[f32]| -> Vec<f32> {
        let mut v = vec![area.top()];
        v.extend(cuts.iter().map(|c| lerp(area.top(), area.bottom(), *c)));
        v.push(area.bottom());
        v
    };
    // The span `[i]` of `edges`, pulled in by half a gap at every inner edge.
    let span = |edges: &[f32], i: usize| -> (f32, f32) {
        let lo = edges[i] + if i > 0 { GAP / 2.0 } else { 0.0 };
        let hi = edges[i + 1] - if i + 2 < edges.len() { GAP / 2.0 } else { 0.0 };
        (lo, hi.max(lo))
    };
    let rect = |x: (f32, f32), y: (f32, f32)| Rect::from_min_max(pos2(x.0, y.0), pos2(x.1, y.1));
    let ratios = if valid_ratios(layout, ratios) {
        ratios.to_vec()
    } else {
        default_ratios(layout)
    };
    let (cols, rows) = match layout {
        SplitLayout::Single => (xs(&[]), ys(&[])),
        SplitLayout::Columns2 | SplitLayout::Columns3 => (xs(&ratios), ys(&[])),
        SplitLayout::Rows2 => (xs(&[]), ys(&ratios)),
        SplitLayout::Grid4 => (xs(&ratios[..1]), ys(&ratios[1..])),
    };
    (0..slot_count(layout))
        .map(|slot| {
            let (c, r) = cell(layout, slot);
            rect(span(&cols, c), span(&rows, r))
        })
        .collect()
}

/// The dividers of `layout` in `area`, matching [`Split::ratios`] by index.
pub fn dividers(layout: SplitLayout, area: Rect, ratios: &[f32]) -> Vec<Divider> {
    let ratios = if valid_ratios(layout, ratios) {
        ratios.to_vec()
    } else {
        default_ratios(layout)
    };
    let vertical = |index: usize, r: f32| {
        let x = lerp(area.left(), area.right(), r);
        Divider {
            index,
            vertical: true,
            rect: Rect::from_min_max(
                pos2(x - GAP / 2.0, area.top()),
                pos2(x + GAP / 2.0, area.bottom()),
            ),
        }
    };
    let horizontal = |index: usize, r: f32| {
        let y = lerp(area.top(), area.bottom(), r);
        Divider {
            index,
            vertical: false,
            rect: Rect::from_min_max(
                pos2(area.left(), y - GAP / 2.0),
                pos2(area.right(), y + GAP / 2.0),
            ),
        }
    };
    match layout {
        SplitLayout::Single => Vec::new(),
        SplitLayout::Columns2 | SplitLayout::Columns3 => ratios
            .iter()
            .enumerate()
            .map(|(i, r)| vertical(i, *r))
            .collect(),
        SplitLayout::Rows2 => vec![horizontal(0, ratios[0])],
        SplitLayout::Grid4 => vec![vertical(0, ratios[0]), horizontal(1, ratios[1])],
    }
}

/// The ratio a divider takes when the pointer is at `pos` inside `area`.
pub fn ratio_at(divider: &Divider, area: Rect, pos: Pos2) -> f32 {
    let (offset, extent) = if divider.vertical {
        (pos.x - area.left(), area.width())
    } else {
        (pos.y - area.top(), area.height())
    };
    if extent <= 0.0 { 0.5 } else { offset / extent }
}

/// Size of a pane's body: the pane minus its header.
pub fn body_of(pane: Rect, header: f32) -> Rect {
    let top = (pane.top() + header).min(pane.bottom());
    Rect::from_min_max(pos2(pane.left(), top), pane.max)
}

/// Pane header strip.
pub fn header_of(pane: Rect, header: f32) -> Rect {
    Rect::from_min_size(pane.min, vec2(pane.width(), header.min(pane.height())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use SplitLayout::*;

    fn two() -> Split {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s
    }

    #[test]
    fn a_shown_tab_takes_focus_instead_of_duplicating() {
        let mut s = two();
        s.show(1);
        assert_eq!(s.slots(), &[Some(1), Some(2)]);
        assert_eq!(s.focused_uid(), Some(1));
    }

    #[test]
    fn a_hidden_tab_replaces_the_focused_pane_only() {
        let mut s = two();
        s.focus_slot(0);
        s.show(3);
        assert_eq!(s.slots(), &[Some(3), Some(2)]);
        assert_eq!(s.focused(), 0);
    }

    #[test]
    fn splitting_right_and_down_grows_the_layout_beside_the_focused_pane() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        assert_eq!((s.layout(), s.focused()), (Columns2, 1));
        // Down from the right column: the new pane sits under it.
        s.open_split(Some(3), Side::Down);
        assert_eq!(s.layout(), Grid4);
        assert_eq!(s.slots(), &[Some(1), Some(2), None, Some(3)]);
        assert_eq!(s.focused(), 3);

        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Down);
        assert_eq!((s.layout(), s.slots()), (Rows2, &[Some(1), Some(2)][..]));
    }

    #[test]
    fn a_third_column_goes_next_to_the_focused_one() {
        let mut s = two();
        s.focus_slot(0);
        s.open_split(Some(3), Side::Right);
        assert_eq!(s.layout(), Columns3);
        assert_eq!(s.slots(), &[Some(1), Some(3), Some(2)]);
        assert_eq!(s.focused(), 1);
        assert_eq!(s.ratios().len(), 2);
    }

    #[test]
    fn rows_grow_into_a_grid_with_the_new_pane_beside_the_focused_row() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Down);
        s.open_split(Some(3), Side::Right);
        assert_eq!(s.slots(), &[Some(1), None, Some(2), Some(3)]);
    }

    #[test]
    fn opening_a_visible_tab_in_a_split_only_focuses_it() {
        let mut s = two();
        s.open_split(Some(1), Side::Down);
        assert_eq!(s.layout(), Columns2);
        assert_eq!(s.focused_uid(), Some(1));
    }

    #[test]
    fn at_four_panes_the_least_recently_focused_other_pane_is_replaced() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Right);
        s.open_split(Some(4), Side::Right);
        assert_eq!(s.layout(), Grid4);
        assert_eq!(s.slots(), &[Some(1), Some(2), Some(3), Some(4)]);
        // Focus history: 4 (now); touching 1, 2, 4 leaves 3 the oldest.
        s.focus_slot(0);
        s.focus_slot(1);
        s.focus_slot(3);
        s.open_split(Some(5), Side::Right);
        assert_eq!(s.slots(), &[Some(1), Some(2), Some(5), Some(4)]);
        assert_eq!(s.focused(), 2);
    }

    #[test]
    fn at_four_panes_an_empty_pane_is_used_before_replacing_a_tab() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Down);
        assert_eq!(s.slots(), &[Some(1), Some(2), None, Some(3)]);
        s.focus_slot(1);
        s.open_split(Some(4), Side::Right);
        assert_eq!(s.slots(), &[Some(1), Some(2), Some(4), Some(3)]);
    }

    #[test]
    fn closing_panes_collapses_the_layout_and_keeps_focus_on_its_tab() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Right);
        s.open_split(Some(4), Side::Right);
        s.focus_slot(3);
        s.close_slot(1);
        assert_eq!(s.layout(), Columns3);
        assert_eq!(s.slots(), &[Some(1), Some(3), Some(4)]);
        assert_eq!(s.focused_uid(), Some(4), "focus followed its tab");
        s.close_slot(2);
        assert_eq!(s.layout(), Columns2);
        assert_eq!(s.focused_uid(), Some(3), "the neighbour takes focus");
        s.close_slot(0);
        assert_eq!(s.layout(), Single);
        assert_eq!(s.slots(), &[Some(3)]);
        assert_eq!(s.focused_uid(), Some(3));
        assert!(s.ratios().is_empty());
    }

    #[test]
    fn closing_the_focused_pane_prefers_a_neighbour_that_shows_a_tab() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(None, Side::Right); // inserted after the focused pane
        assert_eq!(s.slots(), &[Some(1), Some(2), None]);
        s.close_slot(2);
        assert_eq!(s.focused_uid(), Some(2));
    }

    #[test]
    fn an_empty_or_reserved_pane_never_takes_focus_from_a_terminal() {
        let mut s = two();
        s.focus_slot(1);
        s.open_split(None, Side::Right);
        assert_eq!(s.layout(), Columns3);
        assert_eq!(
            s.focused_uid(),
            Some(2),
            "empty pane: focus stays on the tab"
        );
        let mut s = two();
        let t = s.reserve_split(Side::Down);
        assert_eq!(
            s.focused_uid(),
            Some(2),
            "reserved pane: focus stays on the tab"
        );
        assert!(s.is_reserved(3) && s.tickets().eq([t]));
        assert!(s.uids().eq([1, 2]), "a reservation is not a tab");
        // The launch arrives: that exact pane gets the tab and the focus follows via show().
        assert!(s.fill_reserved(t, 7));
        s.show(7);
        assert_eq!(s.slots(), &[Some(1), Some(2), None, Some(7)]);
        assert_eq!(s.focused_uid(), Some(7));
    }

    #[test]
    fn a_launch_fills_its_own_pane_not_the_first_empty_one() {
        // Split down from the right column: empty panes 2 and 3, the reserved one is 3.
        let mut s = two();
        s.focus_slot(1);
        let t = s.reserve_split(Side::Down);
        assert_eq!(s.slots()[2], None);
        assert!(s.fill_reserved(t, 7));
        assert_eq!(s.slots(), &[Some(1), Some(2), None, Some(7)]);
        // A launch nobody reserved a pane for takes the first empty one.
        assert!(s.fill_vacated(8));
        assert_eq!(s.slots(), &[Some(1), Some(2), Some(8), Some(7)]);
    }

    #[test]
    fn concurrent_restarts_each_get_their_own_pane_back() {
        let mut s = two();
        let a = s.reserve_slot_of(1).unwrap();
        let b = s.reserve_slot_of(2).unwrap();
        assert_ne!(a, b);
        assert_eq!(s.focused_uid(), None, "nothing left to focus");
        // The second restart finishes first.
        assert!(s.fill_reserved(b, 20));
        assert!(s.fill_reserved(a, 10));
        assert_eq!(s.slots(), &[Some(10), Some(20)]);
    }

    #[test]
    fn a_restarted_tabs_pane_hands_focus_to_a_pane_with_a_tab() {
        let mut s = two();
        s.focus_slot(0);
        let a = s.reserve_slot_of(1).unwrap();
        assert_eq!(s.focused_uid(), Some(2));
        assert!(s.fill_reserved(a, 10));
        assert_eq!(s.slots(), &[Some(10), Some(2)]);
    }

    #[test]
    fn a_reservation_follows_its_pane_through_layout_changes() {
        let mut s = two();
        let t = s.reserve_slot_of(1).unwrap(); // slot 0
        s.focus_slot(1);
        s.open_split(Some(3), Side::Right); // Columns3: [t, 2, 3]? new pane goes after the focus
        assert_eq!(s.slots(), &[Some(t), Some(2), Some(3)]);
        s.open_split(Some(4), Side::Down); // Grid4
        assert!(s.slots().contains(&Some(t)));
        assert!(s.fill_reserved(t, 9));
        assert_eq!(s.slot_of(9), Some(0));
    }

    #[test]
    fn a_missing_reservation_falls_back_to_the_first_empty_pane() {
        let mut s = two();
        s.open_split(None, Side::Right);
        let t = s.reserve_split(Side::Right);
        s.release(t);
        assert!(!s.tickets().any(|x| x == t));
        assert!(s.fill_reserved(t, 5), "the empty pane takes it");
        assert!(s.slot_of(5).is_some());
        // With nothing empty the tab is left for the caller to show.
        let mut s = two();
        assert!(!s.fill_reserved(99, 5));
        assert_eq!(s.slot_of(5), None);
    }

    #[test]
    fn releasing_a_reservation_collapses_its_pane() {
        let mut s = two();
        let t = s.reserve_split(Side::Right);
        assert_eq!(s.layout(), Columns3);
        s.release(t);
        assert_eq!(s.layout(), Columns2);
        assert_eq!(s.slots(), &[Some(1), Some(2)]);
        assert_eq!(s.focused_uid(), Some(2));
    }

    #[test]
    fn a_full_grid_replaces_other_panes_before_a_reserved_one() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Right);
        let t = s.reserve_split(Side::Right); // Grid4
        assert!(s.is_reserved(3) || s.tickets().eq([t]));
        s.focus_slot(0);
        s.open_split(Some(5), Side::Right);
        assert!(
            s.tickets().eq([t]),
            "the reserved pane survives: {:?}",
            s.slots()
        );
    }

    #[test]
    fn closing_a_tab_also_drops_placeholder_panes() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(None, Side::Right);
        s.open_split(None, Side::Down);
        assert!(s.slots().iter().any(Option::is_none));
        assert!(s.remove_uid(1));
        assert_eq!(s.slots(), &[Some(2)]);
        assert_eq!(s.layout(), Single);
    }

    #[test]
    fn removing_a_tab_without_a_pane_changes_nothing() {
        let mut s = two();
        let before = s.clone();
        assert!(!s.remove_uid(9));
        assert_eq!(s, before);
        assert!(s.remove_uid(2));
        assert_eq!((s.layout(), s.slots()), (Single, &[Some(1)][..]));
    }

    #[test]
    fn a_vacated_pane_is_refilled_by_the_next_tab_and_collapse_is_deferred() {
        let mut s = two();
        s.vacate(1);
        assert_eq!(s.layout(), Columns2);
        assert_eq!(s.slots(), &[None, Some(2)]);
        assert!(!s.fill_vacated(2), "already shown");
        assert!(s.fill_vacated(7));
        assert_eq!(s.slots(), &[Some(7), Some(2)]);
        assert!(!s.fill_vacated(8), "no empty pane left");
    }

    #[test]
    fn retain_drops_dead_tabs() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Right);
        s.retain(|u| u != 2);
        assert_eq!((s.layout(), s.slots()), (Columns2, &[Some(1), Some(3)][..]));
        let mut single = Split::default();
        single.show(5);
        single.retain(|_| false);
        assert_eq!(single.slots(), &[None]);
    }

    #[test]
    fn focus_moves_by_direction_and_stops_at_edges() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Down); // grid: 1 2 / _ 3, focus on 3
        assert_eq!(s.focused(), 3);
        assert!(!s.move_focus(Dir::Right));
        assert!(!s.move_focus(Dir::Down));
        assert!(s.move_focus(Dir::Left));
        assert_eq!(s.focused(), 2);
        assert!(s.move_focus(Dir::Up));
        assert_eq!(s.focused(), 0);
        assert!(!s.move_focus(Dir::Up));
        assert!(!s.move_focus(Dir::Left));
        assert!(s.move_focus(Dir::Right));
        assert_eq!(s.focused(), 1);
    }

    #[test]
    fn rows_move_vertically_only() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Down);
        assert!(!s.move_focus(Dir::Left));
        assert!(!s.move_focus(Dir::Right));
        assert!(s.move_focus(Dir::Up));
        assert_eq!(s.focused_uid(), Some(1));
    }

    #[test]
    fn cycling_wraps_both_ways() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Right);
        s.focus_slot(0);
        s.cycle_focus(-1);
        assert_eq!(s.focused(), 2);
        s.cycle_focus(1);
        assert_eq!(s.focused(), 0);
    }

    #[test]
    fn grid_keeps_panes_and_fills_empties_with_spare_tabs() {
        let mut s = two();
        s.make_grid(&[2, 8, 9, 10]);
        assert_eq!(s.layout(), Grid4);
        assert_eq!(s.slots(), &[Some(1), Some(2), Some(8), Some(9)]);
        assert_eq!(s.focused_uid(), Some(2));
        let mut one = Split::default();
        one.show(1);
        one.make_grid(&[]);
        assert_eq!(one.slots(), &[Some(1), None, None, None]);
    }

    #[test]
    fn unsplit_keeps_the_focused_pane() {
        let mut s = two();
        s.focus_slot(0);
        s.unsplit();
        assert_eq!(
            (s.layout(), s.slots(), s.focused()),
            (Single, &[Some(1)][..], 0)
        );
    }

    #[test]
    fn dividers_stay_clear_of_each_other_and_the_edges() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Right);
        s.set_divider(0, 0.99);
        assert!((s.ratios()[0] - (s.ratios()[1] - MIN_SHARE)).abs() < 1e-6);
        s.set_divider(1, 0.0);
        assert!((s.ratios()[1] - (s.ratios()[0] + MIN_SHARE)).abs() < 1e-6);
        s.set_divider(0, f32::NAN);
        s.set_divider(5, 0.5);
        assert!(valid_ratios(Columns3, s.ratios()));
    }

    #[test]
    fn persistence_maps_panes_to_session_tab_indices() {
        let mut s = Split::default();
        s.show(10);
        s.open_split(Some(20), Side::Right);
        s.open_split(Some(30), Side::Down);
        s.set_divider(1, 0.4);
        // Tab 20 is an update tab: not persisted. Order of open_tabs: 30, 10.
        let state = s.to_state(&[30, 10]).expect("split state");
        assert_eq!(state.layout, Grid4);
        assert_eq!(state.panes, vec![Some(1), None, None, Some(0)]);
        assert_eq!(state.focused, 3);
        assert_eq!(state.ratios, vec![0.5, 0.4]);
        assert!(Split::default().to_state(&[]).is_none());

        // Restored under fresh uids; the pane whose tab is gone collapses.
        let uids = [101, 102];
        let back = Split::from_state(&state, |i| uids.get(i).copied());
        assert_eq!(back.layout(), Columns2);
        assert_eq!(back.slots(), &[Some(102), Some(101)]);
        assert_eq!(back.focused_uid(), Some(101), "focus follows its tab");
    }

    #[test]
    fn a_full_persisted_layout_round_trips_exactly() {
        let mut s = Split::default();
        s.show(1);
        s.open_split(Some(2), Side::Right);
        s.open_split(Some(3), Side::Right);
        s.set_divider(0, 0.25);
        s.focus_slot(0);
        let state = s.to_state(&[1, 2, 3]).expect("split state");
        let back = Split::from_state(&state, |i| Some([1, 2, 3][i]));
        assert_eq!(back.slots(), s.slots());
        assert_eq!(back.ratios(), s.ratios());
        assert_eq!(back.focused(), 0);
    }

    #[test]
    fn restore_survives_garbage() {
        let state = SplitState {
            layout: Grid4,
            panes: vec![Some(0), Some(0), Some(7)],
            focused: 99,
            ratios: vec![f32::NAN],
        };
        let back = Split::from_state(&state, |i| (i < 2).then_some(i as u64 + 1));
        // Index 0 once (duplicate dropped), index 7 dangling, missing slot: one pane left.
        assert_eq!(back.layout(), Single);
        assert_eq!(back.slots(), &[Some(1)]);
        assert_eq!(back.focused(), 0);

        let empty = Split::from_state(&state, |_| None);
        assert_eq!(empty.layout(), Single);
        assert_eq!(empty.slots(), &[None]);
    }

    fn area() -> Rect {
        Rect::from_min_size(pos2(10.0, 20.0), vec2(1000.0, 600.0))
    }

    #[test]
    fn pane_rects_tile_the_area_with_a_gap_and_no_overlap() {
        for layout in [Single, Columns2, Rows2, Columns3, Grid4] {
            let rects = pane_rects(layout, area(), &default_ratios(layout));
            assert_eq!(rects.len(), slot_count(layout));
            let union = rects.iter().fold(rects[0], |a, b| a.union(*b));
            assert_eq!(union, area(), "{layout:?} reaches every edge");
            for (i, a) in rects.iter().enumerate() {
                assert!(a.width() > 0.0 && a.height() > 0.0);
                for b in &rects[i + 1..] {
                    assert!(!a.intersects(*b), "{layout:?}: {a:?} overlaps {b:?}");
                }
            }
        }
        let cols = pane_rects(Columns2, area(), &[0.5]);
        assert!((cols[1].left() - cols[0].right() - GAP).abs() < 1e-3);
    }

    #[test]
    fn grid_panes_share_row_and_column_cuts() {
        let r = pane_rects(Grid4, area(), &[0.3, 0.6]);
        assert_eq!(r[0].right(), r[2].right());
        assert_eq!(r[1].left(), r[3].left());
        assert_eq!(r[0].bottom(), r[1].bottom());
        assert_eq!(r[2].top(), r[3].top());
        assert!(r[0].width() < r[1].width());
        assert!(r[0].height() > r[2].height());
    }

    #[test]
    fn divider_drag_maps_pointer_to_ratio_and_back_to_the_same_cut() {
        let ds = dividers(Grid4, area(), &[0.3, 0.6]);
        assert_eq!(ds.len(), 2);
        assert!(ds[0].vertical && !ds[1].vertical);
        let x = area().left() + 0.3 * area().width();
        assert!((ratio_at(&ds[0], area(), pos2(x, 100.0)) - 0.3).abs() < 1e-5);
        let y = area().top() + 0.6 * area().height();
        assert!((ratio_at(&ds[1], area(), pos2(0.0, y)) - 0.6).abs() < 1e-5);
        // The grab area sits exactly in the gap between the panes.
        let r = pane_rects(Grid4, area(), &[0.3, 0.6]);
        assert!((ds[0].rect.left() - r[0].right()).abs() < 1e-3);
        assert!((ds[0].rect.right() - r[1].left()).abs() < 1e-3);
    }

    #[test]
    fn invalid_ratios_fall_back_to_defaults_in_geometry() {
        let a = pane_rects(Columns3, area(), &[0.9, 0.1]);
        let b = pane_rects(Columns3, area(), &default_ratios(Columns3));
        assert_eq!(a, b);
    }

    #[test]
    fn header_and_body_partition_a_pane() {
        let pane = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let h = header_of(pane, 22.0);
        let b = body_of(pane, 22.0);
        assert_eq!(h.bottom(), b.top());
        assert_eq!(b.bottom(), pane.bottom());
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), vec2(50.0, 10.0));
        assert_eq!(body_of(tiny, 22.0).height(), 0.0);
    }
}

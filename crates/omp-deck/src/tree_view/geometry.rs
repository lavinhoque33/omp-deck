//! Variable-height row geometry: per-class row heights, prefix-sum offsets (rebuilt with
//! the layout), binary search for the visible range, and the graph/card coordinates of the mockup.

use egui::{Pos2, Rect, pos2, vec2};

use crate::tree_layout::RowClass;

/// Card heights of the mockup: message/default rows 38 px, tool rows 28 px.
pub fn row_height(class: RowClass) -> f32 {
    match class {
        RowClass::Tool => 28.0,
        _ => 38.0,
    }
}

/// Slot a row occupies in the scroll content: its card plus the 5 px gap between cards.
pub fn row_slot(class: RowClass) -> f32 {
    row_height(class) + CARD_GAP
}

/// Gap between cards.
pub const CARD_GAP: f32 = 5.0;
const LANE_W: f32 = 18.0;
const FULL_LANES: usize = 7;
/// Left padding of the gutter, and the gap between the gutter and the cards.
pub const PAD_L: f32 = 14.0;
const CARD_PAD: f32 = 10.0;
/// Tool rows sit this much further right than message rows.
pub const TOOL_INDENT: f32 = 21.0;

/// Lane spacing and gutter width for `lanes` lanes: wide trees compress instead of growing.
pub fn gutter(lanes: u16) -> (f32, f32) {
    let n = usize::from(lanes.max(1));
    let step = if n <= FULL_LANES {
        LANE_W
    } else {
        (FULL_LANES as f32 * LANE_W / n as f32).max(6.0)
    };
    (step, n as f32 * step)
}

/// `[first, last)`: rows whose slots intersect the viewport `min..max`, by binary search over the
/// prefix sums (`offsets[r]` = top of row `r`; `offsets.len() == rows + 1`).
pub fn visible_range(offsets: &[f32], min: f32, max: f32) -> (usize, usize) {
    let n = offsets.len().saturating_sub(1);
    if n == 0 {
        return (0, 0);
    }
    // First row whose bottom is below the viewport top.
    let first = offsets[1..].partition_point(|&b| b <= min).min(n);
    // First row whose top is at/below the viewport bottom, plus one row of margin.
    let last = (offsets[..n].partition_point(|&t| t < max) + 1).min(n);
    (first.min(last), last)
}
/// Coordinates of the list area: the prefix sums give each row's slot; the card sits right of the
/// gutter (tool cards indented further), centred vertically in its slot.
pub struct Geometry<'a> {
    left: f32,
    top: f32,
    step: f32,
    graph_w: f32,
    list_w: f32,
    offsets: &'a [f32],
}

impl<'a> Geometry<'a> {
    pub fn new(
        left: f32,
        top: f32,
        step: f32,
        graph_w: f32,
        list_w: f32,
        offsets: &'a [f32],
    ) -> Geometry<'a> {
        Geometry {
            left,
            top,
            step,
            graph_w,
            list_w,
            offsets,
        }
    }

    fn slot_top(&self, row: usize) -> f32 {
        self.top + self.offsets.get(row).copied().unwrap_or(0.0)
    }

    /// The row's slot: full width, from its card top to the next card top.
    pub fn slot(&self, row: usize) -> Rect {
        let top = self.slot_top(row);
        let bottom = self.top + self.offsets.get(row + 1).copied().unwrap_or(top);
        Rect::from_min_size(
            pos2(self.left, top),
            vec2(self.list_w, (bottom - top).max(1.0)),
        )
    }

    pub fn x(&self, lane: u16) -> f32 {
        self.left + PAD_L + (f32::from(lane) + 0.5) * self.step
    }

    /// Vertical centre of row `row`: the middle of its card.
    pub fn y(&self, row: usize) -> f32 {
        self.slot_top(row) + self.slot_h(row) / 2.0 - CARD_GAP / 2.0
    }

    fn slot_h(&self, row: usize) -> f32 {
        let top = self.offsets.get(row).copied().unwrap_or(0.0);
        let bottom = self.offsets.get(row + 1).copied().unwrap_or(top);
        (bottom - top).max(1.0)
    }

    pub fn at(&self, row: usize, lane: u16) -> Pos2 {
        pos2(self.x(lane), self.y(row))
    }

    /// Left edge of the cards: tool cards are indented further right.
    pub fn card_left(&self, indented: bool) -> f32 {
        self.left + PAD_L + self.graph_w + CARD_PAD + if indented { TOOL_INDENT } else { 0.0 }
    }

    /// The card rect of row `row` of class `class`.
    pub fn card(&self, row: usize, class: RowClass) -> Rect {
        let h = row_height(class);
        let left = self.card_left(class == RowClass::Tool);
        let right = self.left + self.list_w - 10.0;
        Rect::from_min_size(
            pos2(left, self.y(row) - h / 2.0),
            vec2((right - left).max(40.0), h),
        )
    }
}

/// Left edge of message cards relative to the list rect (for hit bookkeeping).
pub fn card_left(geo: &Geometry<'_>) -> f32 {
    geo.card_left(false) - geo.left
}

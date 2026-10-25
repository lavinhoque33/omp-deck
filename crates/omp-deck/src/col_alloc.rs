//! Column width allocation for the hub's tables, as a pure function: measured natural widths
//! plus the available width in, which columns stay and how wide they are out. The views measure
//! their cells once per content change (see `sessions_view::layout`), so nothing here allocates
//! per frame beyond the cached result.

/// Space kept free at the table's right edge (the vertical scrollbar).
pub const RIGHT_GAP: f32 = 8.0;

/// Horizontal padding inside every cell (8 on each side) plus a point of rounding slack.
pub const CELL_PAD: f32 = 17.0;

/// One column as the allocator sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    /// Width the column needs so none of its cells or its header is clipped (cell padding
    /// included). Ignored for the flexible column.
    pub natural: f32,
    /// Position in the drop order when the table is too narrow: lowest goes first, `0` = never.
    pub drop: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Alloc {
    /// Width of each column, parallel to the specs; `0.0` = dropped.
    pub widths: Vec<f32>,
    /// Width of the whole table: lead + every shown column + flexible column.
    pub total: f32,
    /// Even the minimum flexible width did not fit: the table scrolls horizontally.
    pub scrolls: bool,
}

/// Lay the columns of a table `avail` wide out. `lead` is the fixed checkbox column, `flex` the
/// index of the column (the title) that takes whatever remains but never less than `flex_min`.
/// Fixed columns keep their natural width; while the flexible one would fall below its minimum,
/// the lowest-ranked droppable column is removed. Only when nothing droppable is left does the
/// table become wider than `avail`.
pub fn allocate(specs: &[Spec], flex: usize, lead: f32, avail: f32, flex_min: f32) -> Alloc {
    let mut shown = vec![true; specs.len()];
    let fixed = |shown: &[bool]| -> f32 {
        lead + specs
            .iter()
            .zip(shown)
            .enumerate()
            .filter(|(i, (_, on))| *i != flex && **on)
            .map(|(_, (s, _))| s.natural)
            .sum::<f32>()
    };
    while avail - fixed(&shown) - RIGHT_GAP < flex_min {
        let next = specs
            .iter()
            .enumerate()
            .filter(|(i, s)| *i != flex && shown[*i] && s.drop > 0)
            .min_by_key(|(_, s)| s.drop)
            .map(|(i, _)| i);
        let Some(i) = next else { break };
        shown[i] = false;
    }
    let fixed = fixed(&shown);
    let room = avail - fixed - RIGHT_GAP;
    let flex_w = room.max(flex_min);
    let widths = specs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if i == flex {
                flex_w
            } else if shown[i] {
                s.natural
            } else {
                0.0
            }
        })
        .collect();
    Alloc {
        widths,
        total: fixed + flex_w,
        scrolls: room < flex_min,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(natural: f32, drop: u8) -> Spec {
        Spec { natural, drop }
    }

    /// title, size (never), model (2), created (1), status (3)
    fn specs() -> Vec<Spec> {
        vec![s(0.0, 0), s(80.0, 0), s(90.0, 2), s(70.0, 1), s(100.0, 3)]
    }

    #[test]
    fn wide_table_keeps_everything_and_title_takes_the_rest() {
        let a = allocate(&specs(), 0, 30.0, 1000.0, 160.0);
        assert_eq!(
            a.widths,
            vec![1000.0 - 30.0 - 340.0 - RIGHT_GAP, 80.0, 90.0, 70.0, 100.0]
        );
        assert!((a.total - (1000.0 - RIGHT_GAP)).abs() < 1e-3);
        assert!(!a.scrolls);
    }

    #[test]
    fn narrow_table_drops_columns_in_rank_order() {
        // 30 + 80 + 90 + 100 + 8 + 160 = 468 fits without Created (70).
        let a = allocate(&specs(), 0, 30.0, 470.0, 160.0);
        assert_eq!(a.widths[3], 0.0, "Created goes first");
        assert_eq!(a.widths[2], 90.0);
        assert_eq!(a.widths[4], 100.0);
        // Without Model as well: 30 + 80 + 100 + 8 + 160 = 378.
        let a = allocate(&specs(), 0, 30.0, 380.0, 160.0);
        assert_eq!((a.widths[3], a.widths[2]), (0.0, 0.0));
        assert_eq!(a.widths[4], 100.0);
        // Then Status: 30 + 80 + 8 + 160 = 278.
        let a = allocate(&specs(), 0, 30.0, 300.0, 160.0);
        assert_eq!((a.widths[3], a.widths[2], a.widths[4]), (0.0, 0.0, 0.0));
        assert_eq!(a.widths[1], 80.0, "never-dropped columns stay");
        assert!(!a.scrolls);
    }

    #[test]
    fn title_is_never_below_its_minimum_and_scrolling_is_the_last_resort() {
        let a = allocate(&specs(), 0, 30.0, 200.0, 160.0);
        assert_eq!(a.widths[0], 160.0);
        assert_eq!(a.widths[1], 80.0);
        assert!(a.scrolls);
        assert!((a.total - (30.0 + 80.0 + 160.0)).abs() < 1e-3);
    }

    #[test]
    fn nothing_is_dropped_that_fits() {
        let a = allocate(&specs(), 0, 30.0, 30.0 + 340.0 + RIGHT_GAP + 160.0, 160.0);
        assert!(a.widths.iter().all(|w| *w > 0.0));
        assert_eq!(a.widths[0], 160.0);
        assert!(!a.scrolls);
    }
}

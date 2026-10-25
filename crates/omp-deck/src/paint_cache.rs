//! Per-row cache of laid-out text for the hub's tables: a row's cells are laid out once and
//! reused every frame until its content, the palette, the font family or the scale changes,
//! instead of allocating a `String` and a galley per visible cell per frame.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::epaint::text::Galley;

use crate::theme::{self, Palette};

#[derive(Default)]
pub struct PaintCache {
    key: u64,
    valid: bool,
    galleys: Vec<Arc<Galley>>,
}

impl PaintCache {
    /// The cached galleys, rebuilt by `build` when `key` differs from the last call.
    pub fn get(&mut self, key: u64, build: impl FnOnce() -> Vec<Arc<Galley>>) -> &[Arc<Galley>] {
        if !self.valid || self.key != key {
            self.galleys = build();
            self.key = key;
            self.valid = true;
        }
        &self.galleys
    }
}

/// Everything besides the row's own content that changes how its text looks: the palette, the
/// UI font family and the scale. Combine with the row's revision via [`with`].
pub fn style_key(ctx: &egui::Context, pal: &Palette) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (pal as *const Palette as usize).hash(&mut h);
    theme::ui_family().hash(&mut h);
    ctx.pixels_per_point().to_bits().hash(&mut h);
    h.finish()
}

/// Mix a content revision (or any per-row state) into a style key.
pub fn with(style: u64, part: impl Hash) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    style.hash(&mut h);
    part.hash(&mut h);
    h.finish()
}

//! Terminal font families: `term`, `term-bold`, `term-primary` (docs/notes/egui-fonts.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};

use egui::epaint::text::Blob;
use egui::epaint::text::{FallbackRequest, FontInsert, FontProvider};
use egui::{FontData, FontDefinitions, FontFamily};
use egui_system_font_provider::SystemFontProvider;

pub(crate) const FAMILY_TERM: &str = "term";
pub(crate) const FAMILY_BOLD: &str = "term-bold";
pub(crate) const FAMILY_PRIMARY: &str = "term-primary";

/// Result of [`install_fonts`].
#[derive(Clone, Debug)]
pub struct FontReport {
    pub primary: Option<PathBuf>,
    pub loaded: Vec<PathBuf>,
    pub missing: Vec<String>,
}

/// Fallback chain after the primary face: (key, fontconfig family). Each family is resolved
/// through fontconfig, so the files may live anywhere on the system.
const FALLBACKS: &[(&str, &str)] = &[
    ("deck-dejavu-mono", "DejaVu Sans Mono"),
    ("deck-source-code-pro", "Source Code Pro"),
    ("deck-symbols2", "Noto Sans Symbols 2"),
    ("deck-symbols-vf", "Noto Sans Symbols"),
    ("deck-cjk-mono-sc", "Noto Sans Mono CJK SC"),
    ("deck-emoji-color", "Noto Color Emoji"),
    ("deck-emoji-mono", "Noto Emoji"),
];

/// File bytes shared process-wide, so repeated installs (or the same file under two families)
/// never read the 32 MB CJK collection twice.
static FONT_BYTES: LazyLock<Mutex<HashMap<PathBuf, Blob>>> = LazyLock::new(Default::default);

fn load_blob(path: &Path) -> Option<Blob> {
    let mut cache = match FONT_BYTES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(b) = cache.get(path) {
        return Some(b.clone());
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let blob: Blob = Arc::new(bytes);
            cache.insert(path.to_path_buf(), blob.clone());
            Some(blob)
        }
        Err(e) => {
            log::debug!("font {} unreadable: {e}", path.display());
            None
        }
    }
}

fn fc_match(pattern: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let p = PathBuf::from(s.trim());
    p.is_file().then_some(p)
}

/// Parses `fc-match -f '%{family}\n%{index}\n%{file}'` output; accepts it only when `family`
/// is among the matched face's families (fc-match always answers with *something*).
fn parse_family_match(output: &str, family: &str) -> Option<(PathBuf, u32)> {
    let mut lines = output.lines();
    let (fams, index, file) = (lines.next()?, lines.next()?, lines.next()?);
    if !fams
        .split(',')
        .any(|f| f.trim().eq_ignore_ascii_case(family))
    {
        return None;
    }
    // The high 16 bits select a variable-font named instance; the face index is the low half.
    let index = index.trim().parse::<u32>().ok()? & 0xffff;
    let path = PathBuf::from(file.trim());
    path.is_file().then_some((path, index))
}

/// File and face index of the installed font whose family really is `family`.
fn fc_family(family: &str) -> Option<(PathBuf, u32)> {
    let out = Command::new("fc-match")
        .args(["-f", "%{family}\n%{index}\n%{file}", family])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_family_match(std::str::from_utf8(&out.stdout).ok()?, family)
}

/// [`SystemFontProvider`] minus the `term-primary` family, which must stay primary-only:
/// `GlyphCache::fits_row` classifies glyphs against it and a discovered fallback would hide
/// which glyphs the primary face lacks.
struct UiSystemFonts(SystemFontProvider);

impl FontProvider for UiSystemFonts {
    fn font_for(&self, request: &FallbackRequest<'_>) -> Option<FontInsert> {
        if request.family == &FontFamily::Name(FAMILY_PRIMARY.into()) {
            return None;
        }
        self.0.font_for(request)
    }
}

/// Faces from [`FALLBACKS`] appended (in this order) to egui's `Proportional` and `Monospace`
/// families after their defaults, so UI text can show emoji, symbols and CJK.
const UI_FALLBACKS: &[&str] = &[
    "deck-symbols2",
    "deck-symbols-vf",
    "deck-cjk-mono-sc",
    "deck-emoji-color",
    "deck-emoji-mono",
    "deck-dejavu-mono",
];

/// Appends the loaded [`UI_FALLBACKS`] faces to `family`, keeping its existing faces first.
fn append_ui_fallbacks(defs: &mut FontDefinitions, family: FontFamily, loaded: &[String]) {
    let list = defs.families.entry(family).or_default();
    for key in UI_FALLBACKS {
        if loaded.iter().any(|k| k == key) && !list.iter().any(|k| k == key) {
            list.push((*key).to_owned());
        }
    }
}

/// Installs the terminal font families into egui (call once, from the eframe creator).
/// Starts from `FontDefinitions::default()`; egui's UI families keep their default faces first
/// and only gain [`UI_FALLBACKS`] at the end (emoji/symbols/CJK in titles). Glyphs the curated
/// chain lacks come on demand from a system font provider that skips `term-primary`. Also disables
/// sub-pixel binning (glyphs are placed on whole pixels anyway; binning only blurs).
pub fn install_fonts(ctx: &egui::Context) -> FontReport {
    let mut defs = FontDefinitions::default();
    let mut report = FontReport {
        primary: None,
        loaded: Vec::new(),
        missing: Vec::new(),
    };

    let add = |defs: &mut FontDefinitions,
               report: &mut FontReport,
               key: &str,
               path: &Path,
               index: u32|
     -> bool {
        match load_blob(path) {
            Some(blob) => {
                defs.font_data
                    .insert(key.to_owned(), Arc::new(FontData::from_blob(blob, index)));
                if !report.loaded.iter().any(|p| p == path) {
                    report.loaded.push(path.to_path_buf());
                }
                true
            }
            None => {
                report.missing.push(path.display().to_string());
                false
            }
        }
    };

    let primary_path = fc_match("monospace");
    let primary_key = match &primary_path {
        Some(p) if add(&mut defs, &mut report, "deck-primary", p, 0) => {
            report.primary = Some(p.clone());
            Some("deck-primary".to_owned())
        }
        Some(_) => None,
        None => {
            report.missing.push("fc-match monospace".to_owned());
            None
        }
    };
    let bold_key = match fc_match("monospace:bold") {
        Some(p)
            if Some(&p) != primary_path.as_ref()
                && add(&mut defs, &mut report, "deck-primary-bold", &p, 0) =>
        {
            Some("deck-primary-bold".to_owned())
        }
        _ => primary_key.clone(),
    };

    // One fc-match per family, concurrently: these spawns delay the first frame.
    let found: Vec<Option<(PathBuf, u32)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = FALLBACKS
            .iter()
            .map(|(_, family)| scope.spawn(|| fc_family(family)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().ok().flatten())
            .collect()
    });
    let mut chain: Vec<String> = Vec::new();
    for ((key, family), hit) in FALLBACKS.iter().zip(found) {
        match hit {
            Some((path, index)) if add(&mut defs, &mut report, key, &path, index) => {
                chain.push((*key).to_owned());
            }
            _ => report.missing.push((*family).to_owned()),
        }
    }
    // Last resort: egui's bundled monospace stack (Hack + emoji) so the families always resolve.
    let builtin = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let primary = primary_key
        .clone()
        .or_else(|| chain.first().cloned())
        .or_else(|| builtin.first().cloned());

    let build = |first: Option<&String>| -> Vec<String> {
        let mut v: Vec<String> = first.into_iter().cloned().collect();
        for k in chain.iter().chain(builtin.iter()) {
            if !v.contains(k) {
                v.push(k.clone());
            }
        }
        v
    };
    defs.families.insert(
        FontFamily::Name(FAMILY_TERM.into()),
        build(primary.as_ref()),
    );
    defs.families.insert(
        FontFamily::Name(FAMILY_BOLD.into()),
        build(bold_key.as_ref().or(primary.as_ref())),
    );
    defs.families.insert(
        FontFamily::Name(FAMILY_PRIMARY.into()),
        primary.into_iter().collect(),
    );
    // Same `font_data` entries (keyed by name), so no font file is loaded twice.
    append_ui_fallbacks(&mut defs, FontFamily::Proportional, &chain);
    append_ui_fallbacks(&mut defs, FontFamily::Monospace, &chain);

    ctx.set_fonts(defs);
    // Replaces (never appends), so repeated installs keep exactly one provider.
    ctx.set_font_providers(vec![Arc::new(UiSystemFonts(SystemFontProvider::new()))]);
    ctx.all_styles_mut(|s| s.visuals.text_options.subpixel_binning = false);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_fallbacks_append_after_defaults_only_when_loaded() {
        let mut defs = FontDefinitions::default();
        let before = defs.families[&FontFamily::Proportional].clone();
        let loaded: Vec<String> = ["deck-emoji-color", "deck-source-code-pro", "deck-symbols2"]
            .map(String::from)
            .to_vec();
        append_ui_fallbacks(&mut defs, FontFamily::Proportional, &loaded);
        append_ui_fallbacks(&mut defs, FontFamily::Proportional, &loaded);
        let after = &defs.families[&FontFamily::Proportional];
        assert_eq!(&after[..before.len()], &before[..]);
        assert_eq!(
            &after[before.len()..],
            ["deck-symbols2", "deck-emoji-color"]
        );
    }

    #[test]
    fn family_match_requires_the_family_and_masks_instance_bits() {
        let file = std::env::current_exe().unwrap();
        let out = |fams: &str, idx: &str| format!("{fams}\n{idx}\n{}", file.display());
        let hit = parse_family_match(
            &out("Noto Sans Mono CJK SC,Noto", "65538"),
            "noto sans mono cjk sc",
        );
        assert_eq!(hit, Some((file.clone(), 2)));
        assert_eq!(
            parse_family_match(&out("DejaVu Sans", "0"), "Noto Emoji"),
            None
        );
    }
}

//! UI font setup: Adwaita Sans (fallback Cantarell, Noto Sans, then egui's built-ins) as the
//! Proportional face, plus the Material icon font. Must run after `deck_term::install_fonts`,
//! which replaces the whole definition set and owns the `term*` families.

use std::path::PathBuf;
use std::process::Command;

use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use egui::{Context, FontData, FontFamily};

/// Candidates in preference order: (fontconfig family, well-known file paths).
const CANDIDATES: &[(&str, &[&str])] = &[
    (
        "Adwaita Sans",
        &["/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf"],
    ),
    (
        "Cantarell",
        &[
            "/usr/share/fonts/abattis-cantarell-vf-fonts/Cantarell-VF.otf",
            "/usr/share/fonts/abattis-cantarell-fonts/Cantarell-Regular.otf",
        ],
    ),
    (
        "Noto Sans",
        &["/usr/share/fonts/google-noto/NotoSans-Regular.ttf"],
    ),
];

/// `fc-match` hit whose family really is `family` (fc-match always answers with *something*).
fn fc_lookup(family: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{family}\n%{file}", family])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let (fam, file) = text.split_once('\n')?;
    fam.split(',')
        .any(|f| f.trim().eq_ignore_ascii_case(family))
        .then(|| PathBuf::from(file.trim()))
}

fn load_one(family: &str, paths: &[&str]) -> Option<Vec<u8>> {
    paths
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| fc_lookup(family))
        .and_then(|p| std::fs::read(p).ok())
}

/// Font bytes of every available UI candidate, in preference order.
pub struct UiFonts {
    faces: Vec<(&'static str, Vec<u8>)>,
}

impl UiFonts {
    /// Resolves and reads the candidates. Blocking (fc-match, file reads): call before the
    /// event loop starts, never from a frame.
    pub fn load() -> UiFonts {
        let faces = CANDIDATES
            .iter()
            .filter_map(|(family, paths)| {
                let bytes = load_one(family, paths);
                if bytes.is_none() {
                    log::debug!("ui font {family}: not found");
                }
                bytes.map(|b| (*family, b))
            })
            .collect();
        UiFonts { faces }
    }
}

/// Installs icons and the preloaded UI font chain (no I/O). The first face becomes the primary
/// Proportional face; the rest are appended as extra glyph fallbacks.
pub fn install(ctx: &Context, fonts: UiFonts) {
    egui_material_icons::initialize(ctx);
    for (i, (family, bytes)) in fonts.faces.into_iter().enumerate() {
        let priority = if i == 0 {
            FontPriority::Highest
        } else {
            FontPriority::Lowest
        };
        log::info!("ui font {family} ({priority:?})");
        ctx.add_font(FontInsert::new(
            &format!("ui-{i}"),
            FontData::from_owned(bytes),
            vec![InsertFontFamily {
                family: FontFamily::Proportional,
                priority,
            }],
        ));
    }
}

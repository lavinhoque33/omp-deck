//! UI font setup. Neutral style: bundled Geist (variable, OFL) as the primary face of the
//! [`NEUTRAL_FAMILY`] family. Neon style: Adwaita Sans (fallback Cantarell, Noto Sans) as the
//! Proportional family. The Neutral family is rebuilt by [`finalize`] as Geist followed by the
//! whole Proportional chain, so its glyph coverage is a superset of Neon's. The bundled
//! Lucide icon font backs the `lucide` family (and is a Proportional fallback so glyphs in
//! label strings render). Must run after `deck_term::install_fonts`, which replaces the whole
//! definition set and owns the `term*` families.

use std::path::PathBuf;
use std::process::Command;

use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use egui::{Context, FontData, FontDefinitions, FontFamily};

/// Named family of the Neutral style's UI text (Geist first, then the system faces).
pub const NEUTRAL_FAMILY: &str = "ui-neutral";

static GEIST: &[u8] = include_bytes!("../assets/fonts/Geist-VariableFont.ttf");
static LUCIDE: &[u8] = include_bytes!("../assets/fonts/lucide.ttf");

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

/// Font bytes of every available system UI candidate, in preference order.
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

fn neutral_family() -> FontFamily {
    FontFamily::Name(NEUTRAL_FAMILY.into())
}

/// Installs Geist, the icon font and the preloaded system UI faces (no I/O). The first system
/// face becomes the primary Proportional face; the rest are appended as extra glyph fallbacks.
pub fn install(ctx: &Context, fonts: UiFonts) {
    ctx.add_font(FontInsert::new(
        "geist",
        FontData::from_static(GEIST),
        vec![InsertFontFamily {
            family: neutral_family(),
            priority: FontPriority::Highest,
        }],
    ));
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
    let mut lucide = FontData::from_static(LUCIDE);
    lucide.tweak.y_offset_factor = 0.0;
    ctx.add_font(FontInsert::new(
        "lucide",
        lucide,
        vec![
            InsertFontFamily {
                family: FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: FontFamily::Name(crate::icons::FONT_FAMILY.into()),
                priority: FontPriority::Highest,
            },
        ],
    ));
}

/// Makes the Neutral family's coverage a superset of Proportional's: Geist first, then every
/// Proportional face in order (system UI faces, egui's emoji faces, deck-term's symbol / CJK /
/// color-emoji fallbacks, Lucide). Call once from the first frame, when the final definitions
/// (after every queued `add_font`) are readable.
pub fn finalize(ctx: &Context) {
    let mut defs: FontDefinitions = ctx.fonts(|f| f.definitions().clone());
    let mut chain = vec!["geist".to_owned()];
    if let Some(prop) = defs.families.get(&FontFamily::Proportional) {
        chain.extend(prop.iter().filter(|k| *k != "geist").cloned());
    }
    if defs.families.get(&neutral_family()) == Some(&chain) {
        return;
    }
    defs.families.insert(neutral_family(), chain);
    ctx.set_fonts(defs);
}

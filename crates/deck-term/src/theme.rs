//! Terminal color palettes (Breeze-like dark default and light for the Neon style, zinc-based
//! Neutral dark and light) and color resolution for alacritty's `Color` values. No egui types:
//! colors are `Rgb`.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// Which built-in palette a terminal uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TermTheme {
    #[default]
    Dark,
    Light,
    /// Zinc near-black; background equals the Neutral chrome `bg`.
    NeutralDark,
    /// White paper, zinc text, ANSI tuned for contrast on white.
    NeutralLight,
}

const fn rgb(hex: u32) -> Rgb {
    Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}

/// A resolved palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection_bg: Rgb,
    /// ANSI 0..=15.
    pub ansi: [Rgb; 16],
}

impl Palette {
    pub fn for_theme(theme: TermTheme) -> Palette {
        match theme {
            // Konsole "Breeze".
            TermTheme::Dark => Palette {
                foreground: rgb(0xfcfcfc),
                background: rgb(0x232627),
                cursor: rgb(0xfcfcfc),
                selection_bg: rgb(0x2d5c76),
                ansi: [
                    rgb(0x232627),
                    rgb(0xed1515),
                    rgb(0x11d116),
                    rgb(0xf67400),
                    rgb(0x1d99f3),
                    rgb(0x9b59b6),
                    rgb(0x1abc9c),
                    rgb(0xfcfcfc),
                    rgb(0x7f8c8d),
                    rgb(0xc0392b),
                    rgb(0x1cdc9a),
                    rgb(0xfdbc4b),
                    rgb(0x3daee9),
                    rgb(0x8e44ad),
                    rgb(0x16a085),
                    rgb(0xffffff),
                ],
            },
            // Breeze light: dark text on paper, ANSI tuned for contrast on white.
            TermTheme::Light => Palette {
                foreground: rgb(0x232627),
                background: rgb(0xfcfcfc),
                cursor: rgb(0x232627),
                selection_bg: rgb(0xb3d7f0),
                ansi: [
                    rgb(0x232627),
                    rgb(0xc0392b),
                    rgb(0x1f8b3a),
                    rgb(0xb65f00),
                    rgb(0x1d6fc4),
                    rgb(0x8e44ad),
                    rgb(0x138a73),
                    rgb(0xbdc3c7),
                    rgb(0x7f8c8d),
                    rgb(0xe74c3c),
                    rgb(0x27ae60),
                    rgb(0xd68400),
                    rgb(0x3daee9),
                    rgb(0x9b59b6),
                    rgb(0x16a085),
                    rgb(0xffffff),
                ],
            },
            TermTheme::NeutralDark => Palette {
                foreground: rgb(0xe4e4e7),
                background: rgb(0x0b0b0d),
                cursor: rgb(0xfafafa),
                // Teal-900: the accent hue, dark enough that every ANSI foreground stays legible.
                selection_bg: rgb(0x134e4a),
                ansi: [
                    rgb(0x18181b),
                    rgb(0xf87171),
                    rgb(0x4ade80),
                    rgb(0xfacc15),
                    rgb(0x60a5fa),
                    rgb(0xc084fc),
                    rgb(0x22d3ee),
                    rgb(0xd4d4d8),
                    rgb(0x52525b),
                    rgb(0xfca5a5),
                    rgb(0x86efac),
                    rgb(0xfde047),
                    rgb(0x93c5fd),
                    rgb(0xd8b4fe),
                    rgb(0x67e8f9),
                    rgb(0xfafafa),
                ],
            },
            // Normal colors are tailwind 600/700 (>= 4.5:1 on white where the hue allows);
            // brights are one step lighter (500/600) so they stay distinct yet readable.
            TermTheme::NeutralLight => Palette {
                foreground: rgb(0x18181b),
                background: rgb(0xffffff),
                cursor: rgb(0x18181b),
                // Teal-100: light accent tint under dark text.
                selection_bg: rgb(0xccfbf1),
                ansi: [
                    rgb(0x18181b),
                    rgb(0xdc2626),
                    rgb(0x15803d),
                    rgb(0xa16207),
                    rgb(0x2563eb),
                    rgb(0x9333ea),
                    rgb(0x0e7490),
                    rgb(0xa1a1aa),
                    rgb(0x71717a),
                    rgb(0xef4444),
                    rgb(0x16a34a),
                    rgb(0xca8a04),
                    rgb(0x3b82f6),
                    rgb(0xa855f7),
                    rgb(0x0891b2),
                    rgb(0xd4d4d8),
                ],
            },
        }
    }

    /// Palette entry for an alacritty color index (0..=268, `NamedColor` numbering).
    pub fn color(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let i = index - 16;
                let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
                Rgb {
                    r: level(i / 36),
                    g: level((i / 6) % 6),
                    b: level(i % 6),
                }
            }
            232..=255 => {
                let v = (8 + (index - 232) * 10) as u8;
                Rgb { r: v, g: v, b: v }
            }
            i if i == NamedColor::Foreground as usize => self.foreground,
            i if i == NamedColor::Background as usize => self.background,
            i if i == NamedColor::Cursor as usize => self.cursor,
            i if i == NamedColor::BrightForeground as usize => self.foreground,
            i if i == NamedColor::DimForeground as usize => self.dim(self.foreground),
            // DimBlack..=DimWhite (259..=266).
            i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                self.dim(self.ansi[i - NamedColor::DimBlack as usize])
            }
            _ => self.foreground,
        }
    }

    /// SGR 2 (faint): two thirds of the way from the background to `c`, so faint text fades
    /// on light palettes instead of turning darker (bolder) than normal text.
    pub fn dim(&self, c: Rgb) -> Rgb {
        let bg = self.background;
        let f = |v: u8, b: u8| ((u16::from(v) * 2 + u16::from(b)) / 3) as u8;
        Rgb {
            r: f(c.r, bg.r),
            g: f(c.g, bg.g),
            b: f(c.b, bg.b),
        }
    }

    /// The colors a session host answers OSC 4/10/11/12 queries with (see
    /// [`crate::RemoteTx::palette`]): ANSI 0..=15, foreground, background, cursor.
    pub fn host_colors(&self) -> [[u8; 3]; 19] {
        let mut out = [[0; 3]; 19];
        let extra = [self.foreground, self.background, self.cursor];
        for (slot, c) in out.iter_mut().zip(self.ansi.iter().chain(&extra)) {
            *slot = [c.r, c.g, c.b];
        }
        out
    }
}

/// Effective color at `index`: app override (OSC 4/10/11/12) else the palette.
pub fn indexed(colors: &Colors, palette: &Palette, index: usize) -> Rgb {
    colors[index].unwrap_or_else(|| palette.color(index))
}

/// Resolves a cell color. `dimmed` applies SGR 2.
pub fn resolve(color: Color, colors: &Colors, palette: &Palette, dimmed: bool) -> Rgb {
    match color {
        Color::Spec(c) => {
            if dimmed {
                palette.dim(c)
            } else {
                c
            }
        }
        Color::Indexed(i) => {
            let c = indexed(colors, palette, usize::from(i));
            if dimmed { palette.dim(c) } else { c }
        }
        Color::Named(n) => {
            let idx = n as usize;
            if dimmed {
                if idx < 8 {
                    indexed(colors, palette, n.to_dim() as usize)
                } else if n == NamedColor::Foreground {
                    indexed(colors, palette, NamedColor::DimForeground as usize)
                } else {
                    palette.dim(indexed(colors, palette, idx))
                }
            } else {
                indexed(colors, palette, idx)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_gray() {
        let p = Palette::for_theme(TermTheme::Dark);
        assert_eq!(p.color(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(p.color(21), Rgb { r: 0, g: 0, b: 255 });
        assert_eq!(p.color(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(
            p.color(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(p.color(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            p.color(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
        assert_eq!(p.color(257), p.background);
        assert_eq!(p.color(256), p.foreground);
        assert_eq!(p.color(258), p.cursor);
    }

    #[test]
    fn overrides_win() {
        let p = Palette::for_theme(TermTheme::Light);
        let mut colors = Colors::default();
        colors[1] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(
            resolve(Color::Indexed(1), &colors, &p, false),
            Rgb { r: 1, g: 2, b: 3 }
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors, &p, false),
            Rgb { r: 1, g: 2, b: 3 }
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Green), &colors, &p, false),
            p.ansi[2]
        );
        assert_eq!(
            resolve(Color::Named(NamedColor::Green), &colors, &p, true),
            p.dim(p.ansi[2])
        );
    }

    #[test]
    fn neutral_osc_replies_follow_palette() {
        // OSC 10/11/12/4 replies resolve through `indexed` with no app overrides.
        let colors = Colors::default();
        let dark = Palette::for_theme(TermTheme::NeutralDark);
        let bg = NamedColor::Background as usize;
        let fg = NamedColor::Foreground as usize;
        assert_eq!(indexed(&colors, &dark, bg), rgb(0x0b0b0d));
        assert_eq!(indexed(&colors, &dark, fg), rgb(0xe4e4e7));
        assert_eq!(
            indexed(&colors, &dark, NamedColor::Cursor as usize),
            rgb(0xfafafa)
        );
        assert_eq!(indexed(&colors, &dark, 4), rgb(0x60a5fa));
        assert_eq!(indexed(&colors, &dark, 15), rgb(0xfafafa));
        let light = Palette::for_theme(TermTheme::NeutralLight);
        assert_eq!(indexed(&colors, &light, bg), rgb(0xffffff));
        assert_eq!(indexed(&colors, &light, fg), rgb(0x18181b));
        // SGR 2 on a named color goes through the dim slots of the active palette.
        assert_eq!(
            resolve(Color::Named(NamedColor::Red), &colors, &light, true),
            light.dim(rgb(0xdc2626))
        );
    }

    #[test]
    fn faint_fades_toward_background() {
        let colors = Colors::default();
        // On white, faint black text gets lighter (not darker) than normal text.
        let light = Palette::for_theme(TermTheme::NeutralLight);
        let faint = resolve(Color::Named(NamedColor::Foreground), &colors, &light, true);
        assert_eq!(faint, rgb(0x656567));
        // On near-black, faint text gets darker.
        let dark = Palette::for_theme(TermTheme::NeutralDark);
        let faint = resolve(Color::Spec(rgb(0xffffff)), &colors, &dark, true);
        assert_eq!(faint, rgb(0xadadae));
    }
}

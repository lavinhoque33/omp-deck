//! Terminal color palettes (Breeze-like dark default, Breeze-like light) and color resolution
//! for alacritty's `Color` values. No egui types: colors are `Rgb`.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// Which built-in palette a terminal uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TermTheme {
    #[default]
    Dark,
    Light,
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
            i if i == NamedColor::DimForeground as usize => dim(self.foreground),
            // DimBlack..=DimWhite (259..=266).
            i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                dim(self.ansi[i - NamedColor::DimBlack as usize])
            }
            _ => self.foreground,
        }
    }
}

/// SGR 2 (faint): two thirds intensity.
pub fn dim(c: Rgb) -> Rgb {
    let f = |v: u8| ((u16::from(v) * 2) / 3) as u8;
    Rgb {
        r: f(c.r),
        g: f(c.g),
        b: f(c.b),
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
                dim(c)
            } else {
                c
            }
        }
        Color::Indexed(i) => {
            let c = indexed(colors, palette, usize::from(i));
            if dimmed { dim(c) } else { c }
        }
        Color::Named(n) => {
            let idx = n as usize;
            if dimmed {
                if idx < 8 {
                    indexed(colors, palette, n.to_dim() as usize)
                } else if n == NamedColor::Foreground {
                    indexed(colors, palette, NamedColor::DimForeground as usize)
                } else {
                    dim(indexed(colors, palette, idx))
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
            dim(p.ansi[2])
        );
    }
}

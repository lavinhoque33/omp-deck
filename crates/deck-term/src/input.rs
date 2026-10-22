//! Pure input encoders: keys, mouse reports, paste, focus and alternate scroll.
//!
//! Everything here is a plain function of `(input, TermMode, KeyRules)` → bytes, with no
//! egui types, so every sequence is unit-tested byte-for-byte.
//!
//! Key encoding priority (docs/notes/contracts.md §Key encoding):
//! 1. kitty keyboard flags active in `TermMode` → kitty CSI-u per flags;
//! 2. else [`KeyRules::Tmux`] / [`KeyRules::ModifyOtherKeys`] → modifyOtherKeys
//!    (`CSI 27;mod;code~`) for keys legacy cannot express, Alt+char as `ESC char`, everything
//!    else legacy; under tmux a lone Escape is `ESC O [`;
//! 3. else xterm legacy.

use alacritty_terminal::term::TermMode;

/// Modifier state of a key or mouse event. Super/Meta are invisible to egui on Linux.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: false,
    };
    pub const SHIFT: Mods = Mods {
        shift: true,
        alt: false,
        ctrl: false,
    };
    pub const ALT: Mods = Mods {
        shift: false,
        alt: true,
        ctrl: false,
    };
    pub const CTRL: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: true,
    };

    pub const fn new(shift: bool, alt: bool, ctrl: bool) -> Mods {
        Mods { shift, alt, ctrl }
    }

    /// xterm/kitty modifier parameter: `1 + shift(1) + alt(2) + ctrl(4)`.
    pub fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    pub fn is_empty(self) -> bool {
        !(self.shift || self.alt || self.ctrl)
    }
}

/// A logical key. `Char` carries the unshifted base character (lowercase letter, digit,
/// punctuation, `' '` for Space).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1..=F35.
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

/// One key event. `text` is the printable text the key produced (egui's `Event::Text` that
/// followed the `Event::Key`), authoritative for printable characters including Alt+char and
/// AltGr; `None` for Ctrl combos and non-text keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyInput<'a> {
    pub key: Key,
    pub mods: Mods,
    pub text: Option<&'a str>,
    pub action: KeyAction,
}

impl<'a> KeyInput<'a> {
    pub fn press(key: Key, mods: Mods) -> KeyInput<'a> {
        KeyInput {
            key,
            mods,
            text: None,
            action: KeyAction::Press,
        }
    }

    pub fn with_text(mut self, text: &'a str) -> KeyInput<'a> {
        self.text = Some(text);
        self
    }
}

const ESC: u8 = 0x1b;

/// Which non-kitty rule set applies (contracts.md §Key encoding rules 2/3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRules {
    /// Rule 3: xterm legacy bytes.
    Legacy,
    /// Rule 2 for a direct child that requested modifyOtherKeys level 2 (`CSI > 4 ; 2 m`).
    ModifyOtherKeys,
    /// Rule 2 for a tmux client (tmux always asks the outer terminal for level 2), plus lone
    /// Escape as `ESC O [` (tmux ?7727 application-escape form).
    Tmux,
}

/// Encodes a key event to the bytes the PTY should receive (empty = nothing to send).
pub fn encode_key(input: &KeyInput<'_>, mode: TermMode, rules: KeyRules) -> Vec<u8> {
    let text = input.text.filter(|t| is_printable(t));
    if mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) {
        return encode_kitty(input.key, input.mods, text, input.action, mode);
    }
    if input.action == KeyAction::Release {
        return Vec::new();
    }
    match rules {
        KeyRules::Legacy => encode_legacy(input.key, input.mods, text, mode),
        KeyRules::ModifyOtherKeys => encode_mok(input.key, input.mods, text, mode, false),
        KeyRules::Tmux => encode_mok(input.key, input.mods, text, mode, true),
    }
}

/// Encodes committed text that arrived without a key event (IME commit, compose result).
pub fn encode_text(text: &str) -> Vec<u8> {
    text.bytes().collect()
}

fn is_printable(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| !c.is_control())
}

fn push_num(out: &mut Vec<u8>, n: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    let mut n = n;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// `CSI 27 ; mod ; code ~` (xterm modifyOtherKeys form).
fn modify_other_keys(out: &mut Vec<u8>, mods: Mods, code: u32) {
    out.extend_from_slice(b"\x1b[27;");
    push_num(out, u32::from(mods.param()));
    out.push(b';');
    push_num(out, code);
    out.push(b'~');
}

/// Legacy C0 byte for Ctrl+`c` (xterm). `strict` restricts the table to the mappings tmux
/// round-trips unambiguously (letters, `@`/Space, `[`, `\`, `]`, `^`, `_`).
fn ctrl_byte(c: char, strict: bool) -> Option<u8> {
    let b = match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        ' ' | '@' => 0,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        _ if strict => return None,
        '2' => 0,
        '3' => 0x1b,
        '4' => 0x1c,
        '5' => 0x1d,
        '6' => 0x1e,
        '7' | '/' | '-' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    };
    Some(b)
}

/// Legacy sequences for non-character keys (shared by legacy and tmux modes).
/// Returns false if `key` is not a "special" key.
fn legacy_special(out: &mut Vec<u8>, key: Key, mods: Mods, mode: TermMode) -> bool {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    // Letter-terminated keys: CSI/SS3 <letter>, modified `CSI 1;<mod><letter>`.
    let letter = match key {
        Key::ArrowUp => Some((b'A', true)),
        Key::ArrowDown => Some((b'B', true)),
        Key::ArrowRight => Some((b'C', true)),
        Key::ArrowLeft => Some((b'D', true)),
        Key::Home => Some((b'H', true)),
        Key::End => Some((b'F', true)),
        Key::F(1) => Some((b'P', false)),
        Key::F(2) => Some((b'Q', false)),
        Key::F(3) => Some((b'R', false)),
        Key::F(4) => Some((b'S', false)),
        _ => None,
    };
    if let Some((l, cursor_key)) = letter {
        if mods.is_empty() {
            // Cursor keys honour DECCKM; F1-F4 are always SS3.
            if cursor_key && !app_cursor {
                out.extend_from_slice(&[ESC, b'[', l]);
            } else {
                out.extend_from_slice(&[ESC, b'O', l]);
            }
        } else {
            out.extend_from_slice(b"\x1b[1;");
            push_num(out, u32::from(mods.param()));
            out.push(l);
        }
        return true;
    }
    let tilde = match key {
        Key::Insert => 2,
        Key::Delete => 3,
        Key::PageUp => 5,
        Key::PageDown => 6,
        Key::F(5) => 15,
        Key::F(6) => 17,
        Key::F(7) => 18,
        Key::F(8) => 19,
        Key::F(9) => 20,
        Key::F(10) => 21,
        Key::F(11) => 23,
        Key::F(12) => 24,
        Key::F(13) => 25,
        Key::F(14) => 26,
        Key::F(15) => 28,
        Key::F(16) => 29,
        Key::F(17) => 31,
        Key::F(18) => 32,
        Key::F(19) => 33,
        Key::F(20) => 34,
        Key::F(_) => return true, // F21+ have no legacy encoding: send nothing.
        _ => return false,
    };
    out.extend_from_slice(b"\x1b[");
    push_num(out, tilde);
    if !mods.is_empty() {
        out.push(b';');
        push_num(out, u32::from(mods.param()));
    }
    out.push(b'~');
    true
}

fn enter_bytes(out: &mut Vec<u8>, mode: TermMode) {
    if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
        out.extend_from_slice(b"\r\n");
    } else {
        out.push(b'\r');
    }
}

/// Character key with text/alt/ctrl, legacy rules. `strict_ctrl` selects the tmux C0 subset
/// (returns false when the combo must instead be sent as modifyOtherKeys).
fn legacy_char(
    out: &mut Vec<u8>,
    c: char,
    mods: Mods,
    text: Option<&str>,
    strict_ctrl: bool,
) -> bool {
    if mods.ctrl {
        let Some(b) = ctrl_byte(c, strict_ctrl) else {
            if strict_ctrl {
                return false;
            }
            // xterm sends the plain character for Ctrl+<key without C0 mapping>.
            if mods.alt {
                out.push(ESC);
            }
            push_char(out, c);
            return true;
        };
        if mods.alt {
            out.push(ESC);
        }
        out.push(b);
        return true;
    }
    if mods.alt {
        out.push(ESC);
    }
    match text {
        Some(t) => out.extend_from_slice(t.as_bytes()),
        None if mods.shift => {
            for u in c.to_uppercase() {
                push_char(out, u);
            }
        }
        None => push_char(out, c),
    }
    true
}

fn encode_legacy(key: Key, mods: Mods, text: Option<&str>, mode: TermMode) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    match key {
        Key::Enter => {
            if mods.alt {
                out.push(ESC);
            }
            enter_bytes(&mut out, mode);
        }
        Key::Tab => {
            if mods.shift {
                if mods.alt {
                    out.push(ESC);
                }
                out.extend_from_slice(b"\x1b[Z");
            } else {
                if mods.alt {
                    out.push(ESC);
                }
                out.push(b'\t');
            }
        }
        Key::Backspace => {
            if mods.alt {
                out.push(ESC);
            }
            out.push(if mods.ctrl { 0x08 } else { 0x7f });
        }
        Key::Escape => {
            if mods.alt {
                out.push(ESC);
            }
            out.push(ESC);
        }
        Key::Char(c) => {
            legacy_char(&mut out, c, mods, text, false);
        }
        _ => {
            legacy_special(&mut out, key, mods, mode);
        }
    }
    out
}

/// Rule 2. `tmux` selects the `ESC O [` form for a lone Escape.
fn encode_mok(key: Key, mods: Mods, text: Option<&str>, mode: TermMode, tmux: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    let only_alt = mods == Mods::ALT;
    match key {
        Key::Enter => {
            if mods.is_empty() || only_alt {
                if only_alt {
                    out.push(ESC);
                }
                enter_bytes(&mut out, mode);
            } else {
                modify_other_keys(&mut out, mods, 13);
            }
        }
        Key::Tab => match (mods.shift, mods.alt, mods.ctrl) {
            (false, false, false) => out.push(b'\t'),
            (true, false, false) => out.extend_from_slice(b"\x1b[Z"),
            (false, true, false) => out.extend_from_slice(b"\x1b\t"),
            _ => modify_other_keys(&mut out, mods, 9),
        },
        Key::Backspace => match (mods.shift, mods.alt, mods.ctrl) {
            (false, false, false) => out.push(0x7f),
            (false, true, false) => out.extend_from_slice(b"\x1b\x7f"),
            _ => modify_other_keys(&mut out, mods, 127),
        },
        Key::Escape => match (mods.shift, mods.alt, mods.ctrl) {
            // tmux ?7727 application-escape form: delivered without the 500 ms ESC delay.
            (false, false, false) if tmux => out.extend_from_slice(b"\x1bO["),
            (false, false, false) => out.push(ESC),
            (false, true, false) => out.extend_from_slice(b"\x1b\x1b"),
            _ => modify_other_keys(&mut out, mods, 27),
        },
        Key::Char(c) => {
            // Shift+Ctrl+<char> and Shift+Space cannot be expressed in legacy bytes.
            let ambiguous = mods.shift && (mods.ctrl || c == ' ');
            if ambiguous || !legacy_char(&mut out, c, mods, text, true) {
                out.clear();
                modify_other_keys(&mut out, mods, base_code(c));
            }
        }
        _ => {
            legacy_special(&mut out, key, mods, mode);
        }
    }
    out
}

fn base_code(c: char) -> u32 {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l as u32,
        _ => c as u32,
    }
}

/// Kitty keyboard protocol (https://sw.kovidgoyal.net/kitty/keyboard-protocol/), mirroring
/// kitty's reference encoder (`kitty/key_encoding.c`, `encode_glfw_key_event`):
/// - Without flag 8 (all keys as escapes), text-producing presses and repeats send their text;
///   their *releases* (flag 2) are reported as `CSI code;mods:3u` like in kitty and ghostty
///   (the spec's "no way to be notified" sentence describes the legacy protocol).
/// - Unmodified Enter/Tab/Backspace send legacy bytes and never report releases without
///   flag 8; modified ones are CSI-u with events.
/// - With flag 2, repeat/release of any other key are CSI-u with `:2`/`:3` even when the press
///   itself is legacy (e.g. Ctrl+a press `0x01`, repeat `CSI 97;5:2u`).
/// - Deviation: kitty sends a bare ESC on the *release* of unmodified Escape when flag 1 is off
///   (a reference quirk that doubles Esc); we send nothing for that release.
fn encode_kitty(
    key: Key,
    mods: Mods,
    text: Option<&str>,
    action: KeyAction,
    mode: TermMode,
) -> Vec<u8> {
    let disambiguate = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let event_types = mode.contains(TermMode::REPORT_EVENT_TYPES);
    let alternate = mode.contains(TermMode::REPORT_ALTERNATE_KEYS);
    let all_as_esc = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let assoc_text = mode.contains(TermMode::REPORT_ASSOCIATED_TEXT);
    let release = action == KeyAction::Release;

    if release && !event_types {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(16);
    // Text-producing keys with no modifier other than Shift send their text on press/repeat.
    if !all_as_esc
        && !release
        && !mods.alt
        && !mods.ctrl
        && let (Key::Char(_), Some(t)) = (key, text)
    {
        out.extend_from_slice(t.as_bytes());
        return out;
    }
    let add_actions = event_types && action != KeyAction::Press;
    let event = add_actions.then_some(match action {
        KeyAction::Repeat => 2,
        _ => 3,
    });
    let modp = u32::from(mods.param());

    if !matches!(key, Key::Char(_)) {
        let legacy_mode = !event_types && !disambiguate && !all_as_esc;
        if mods.is_empty() {
            if key == Key::Escape && !disambiguate && !all_as_esc {
                if !release {
                    out.push(ESC);
                }
                return out;
            }
            if legacy_mode
                && matches!(
                    key,
                    Key::ArrowUp
                        | Key::ArrowDown
                        | Key::ArrowLeft
                        | Key::ArrowRight
                        | Key::Home
                        | Key::End
                        | Key::F(1..=4)
                )
            {
                legacy_special(&mut out, key, mods, mode);
                return out;
            }
            if !all_as_esc && matches!(key, Key::Enter | Key::Tab | Key::Backspace) {
                if !release {
                    match key {
                        Key::Enter => enter_bytes(&mut out, mode),
                        Key::Tab => out.push(b'\t'),
                        _ => out.push(0x7f),
                    }
                }
                return out;
            }
        } else if legacy_mode && matches!(key, Key::Enter | Key::Tab | Key::Backspace | Key::Escape)
        {
            // legacy_mode implies no release reaches here.
            return encode_legacy(key, mods, None, mode);
        }
    }

    // Functional keys with a letter or `~` terminator.
    let functional: Option<(u32, u8)> = match key {
        Key::ArrowUp => Some((1, b'A')),
        Key::ArrowDown => Some((1, b'B')),
        Key::ArrowRight => Some((1, b'C')),
        Key::ArrowLeft => Some((1, b'D')),
        Key::Home => Some((1, b'H')),
        Key::End => Some((1, b'F')),
        Key::F(1) => Some((1, b'P')),
        Key::F(2) => Some((1, b'Q')),
        Key::F(3) => Some((13, b'~')),
        Key::F(4) => Some((1, b'S')),
        Key::F(5) => Some((15, b'~')),
        Key::F(6) => Some((17, b'~')),
        Key::F(7) => Some((18, b'~')),
        Key::F(8) => Some((19, b'~')),
        Key::F(9) => Some((20, b'~')),
        Key::F(10) => Some((21, b'~')),
        Key::F(11) => Some((23, b'~')),
        Key::F(12) => Some((24, b'~')),
        Key::Insert => Some((2, b'~')),
        Key::Delete => Some((3, b'~')),
        Key::PageUp => Some((5, b'~')),
        Key::PageDown => Some((6, b'~')),
        _ => None,
    };
    if let Some((num, term)) = functional {
        out.extend_from_slice(b"\x1b[");
        let need_params = modp != 1 || event.is_some();
        if term == b'~' || need_params {
            push_num(&mut out, num);
        }
        if need_params {
            out.push(b';');
            push_num(&mut out, modp);
            if let Some(e) = event {
                out.push(b':');
                push_num(&mut out, e);
            }
        }
        out.push(term);
        return out;
    }

    let (code, shifted) = match key {
        Key::Char(c) => {
            let code = base_code(c);
            let shifted = if mods.shift {
                match text.map(|t| {
                    let mut it = t.chars();
                    (it.next(), it.next())
                }) {
                    Some((Some(s), None)) if s as u32 != code => Some(s as u32),
                    Some(_) => None,
                    None => {
                        let mut up = c.to_uppercase();
                        match (up.next(), up.next()) {
                            (Some(u), None) if u as u32 != code => Some(u as u32),
                            _ => None,
                        }
                    }
                }
            } else {
                None
            };
            (code, shifted)
        }
        Key::Enter => (13, None),
        Key::Tab => (9, None),
        Key::Backspace => (127, None),
        Key::Escape => (27, None),
        // F13..=F35 use kitty's private-use code points.
        Key::F(n) if (13..=35).contains(&n) => (57376 + u32::from(n) - 13, None),
        _ => return out,
    };
    let alternates = alternate.then_some(shifted).flatten();
    let text_field = if assoc_text && all_as_esc && !release {
        text
    } else {
        None
    };
    if let Key::Char(c) = key
        && !add_actions
        && alternates.is_none()
        && text_field.is_none()
    {
        // kitty's "simple encoding": plain key → the key itself (or `CSI code u` with flag 8);
        // legacy-expressible modifier combos stay legacy without flags 1/8.
        if mods.is_empty() && !all_as_esc {
            push_char(&mut out, c);
            return out;
        }
        if !mods.is_empty()
            && !disambiguate
            && !all_as_esc
            && kitty_legacy_printable(&mut out, c, mods, shifted)
        {
            return out;
        }
    }
    out.extend_from_slice(b"\x1b[");
    push_num(&mut out, code);
    if let Some(s) = alternates {
        out.push(b':');
        push_num(&mut out, s);
    }
    if modp != 1 || event.is_some() || text_field.is_some() {
        out.push(b';');
        if modp != 1 || event.is_some() {
            push_num(&mut out, modp);
        }
        if let Some(e) = event {
            out.push(b':');
            push_num(&mut out, e);
        }
        if let Some(t) = text_field {
            out.push(b';');
            for (i, ch) in t.chars().enumerate() {
                if i > 0 {
                    out.push(b':');
                }
                push_num(&mut out, ch as u32);
            }
        }
    }
    out.push(b'u');
    out
}

/// kitty's `encode_printable_ascii_key_legacy`: Shift/Alt/Ctrl/Ctrl+Alt combos of printable
/// ASCII keys that legacy bytes express. Returns false (nothing written) otherwise.
fn kitty_legacy_printable(out: &mut Vec<u8>, c: char, mods: Mods, shifted: Option<u32>) -> bool {
    if !(c == ' ' || (c.is_ascii_graphic() && !c.is_ascii_uppercase())) {
        return false;
    }
    let mut key = c as u8;
    let mut m = mods;
    if m.shift
        && let Some(s) = shifted.and_then(|s| u8::try_from(s).ok())
        && s != key
        && (!m.ctrl || !key.is_ascii_lowercase())
    {
        key = s;
        m.shift = false;
    }
    let ctrled = |k: u8| match k {
        b'-' => b'-',
        b'~' => 0x1e,
        _ => ctrl_byte(char::from(k), false).unwrap_or(k),
    };
    match (m.shift, m.alt, m.ctrl) {
        _ if mods == Mods::SHIFT => out.push(key),
        (false, true, false) => out.extend_from_slice(&[ESC, key]),
        (false, false, true) => out.push(ctrled(key)),
        (false, true, true) => out.extend_from_slice(&[ESC, ctrled(key)]),
        (true, false, true) if key == b' ' => out.push(0),
        (true, true, false) if key == b' ' => out.extend_from_slice(b"\x1b "),
        _ => return false,
    }
    true
}

/// Mouse button of a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Motion without any button held.
    NoButton,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// Whether the application asked for this kind of mouse event.
pub fn mouse_report_wanted(action: MouseAction, button_held: bool, mode: TermMode) -> bool {
    match action {
        MouseAction::Press | MouseAction::Release => mode.intersects(TermMode::MOUSE_MODE),
        MouseAction::Motion if button_held => {
            mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
        }
        MouseAction::Motion => mode.contains(TermMode::MOUSE_MOTION),
    }
}

/// Largest 1-based coordinate the legacy X10 encoding can carry (32 + 223 = 255).
pub const X10_MAX_COORD: usize = 223;
/// Largest 1-based coordinate the UTF-8 (?1005) encoding can carry (32 + 2015 = 2047).
pub const UTF8_MAX_COORD: usize = 2015;

/// Encodes a mouse report at 0-based cell (`col`, `row`). Format by mode: SGR (?1006),
/// UTF-8 (?1005) or legacy X10 (coordinates clamped to 223).
pub fn encode_mouse(
    button: MouseButton,
    action: MouseAction,
    mods: Mods,
    col: usize,
    row: usize,
    mode: TermMode,
) -> Vec<u8> {
    let base: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::NoButton => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
        MouseButton::WheelLeft => 66,
        MouseButton::WheelRight => 67,
    };
    let mod_bits = 4 * u32::from(mods.shift) + 8 * u32::from(mods.alt) + 16 * u32::from(mods.ctrl);
    let motion = if action == MouseAction::Motion { 32 } else { 0 };
    let mut out = Vec::with_capacity(16);
    if mode.contains(TermMode::SGR_MOUSE) {
        out.extend_from_slice(b"\x1b[<");
        push_num(&mut out, base + motion + mod_bits);
        out.push(b';');
        push_num(&mut out, col as u32 + 1);
        out.push(b';');
        push_num(&mut out, row as u32 + 1);
        out.push(if action == MouseAction::Release {
            b'm'
        } else {
            b'M'
        });
        return out;
    }
    // Normal encodings report every release as button 3.
    let code = if action == MouseAction::Release {
        3
    } else {
        base
    } + motion
        + mod_bits;
    out.extend_from_slice(b"\x1b[M");
    if mode.contains(TermMode::UTF8_MOUSE) {
        let enc = |out: &mut Vec<u8>, v: u32| {
            // 32 + v ≤ 2047 is always a valid scalar value.
            if let Some(ch) = char::from_u32(32 + v) {
                push_char(out, ch);
            }
        };
        enc(&mut out, code);
        enc(&mut out, (col + 1).min(UTF8_MAX_COORD) as u32);
        enc(&mut out, (row + 1).min(UTF8_MAX_COORD) as u32);
    } else {
        out.push((32 + code).min(255) as u8);
        out.push(32 + (col + 1).min(X10_MAX_COORD) as u8);
        out.push(32 + (row + 1).min(X10_MAX_COORD) as u8);
    }
    out
}

/// Paste: bracketed (`ESC[200~`…`ESC[201~`, embedded end markers removed) when the app enabled
/// ?2004, else newlines (`\r\n`, `\n`) become `\r`.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    const END: &str = "\x1b[201~";
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut body = text.replace(END, "");
        // Removing one marker can splice a new one together; repeat until none is left.
        while body.contains(END) {
            body = body.replace(END, "");
        }
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(END.as_bytes());
        out
    } else {
        let mut out = Vec::with_capacity(text.len());
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                    out.push(b'\r');
                    i += 1;
                }
                b'\n' => out.push(b'\r'),
                b => out.push(b),
            }
            i += 1;
        }
        out
    }
}

/// Focus report (`ESC[I` / `ESC[O`) if the app enabled ?1004.
pub fn encode_focus(focused: bool, mode: TermMode) -> Option<&'static [u8]> {
    if !mode.contains(TermMode::FOCUS_IN_OUT) {
        return None;
    }
    Some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

/// Alternate scroll (?1007 in the alt screen without mouse mode): `lines` > 0 scrolls up
/// (Up arrows), < 0 down. Arrow form honours APP_CURSOR.
pub fn encode_alt_scroll(lines: i32, mode: TermMode) -> Vec<u8> {
    let letter = if lines > 0 { b'A' } else { b'B' };
    let intro = if mode.contains(TermMode::APP_CURSOR) {
        b'O'
    } else {
        b'['
    };
    let n = lines.unsigned_abs() as usize;
    let mut out = Vec::with_capacity(3 * n);
    for _ in 0..n {
        out.extend_from_slice(&[ESC, intro, letter]);
    }
    out
}

/// Wheel goes to the app as arrow keys (alternate scroll) instead of local scrollback.
pub fn wants_alt_scroll(mode: TermMode) -> bool {
    mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        && !mode.intersects(TermMode::MOUSE_MODE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: TermMode = TermMode::empty();

    fn rules(tmux: bool) -> KeyRules {
        if tmux {
            KeyRules::Tmux
        } else {
            KeyRules::Legacy
        }
    }

    fn key(k: Key, m: Mods, mode: TermMode, tmux: bool) -> Vec<u8> {
        encode_key(&KeyInput::press(k, m), mode, rules(tmux))
    }

    fn txt(k: Key, m: Mods, t: &str, mode: TermMode, tmux: bool) -> Vec<u8> {
        encode_key(&KeyInput::press(k, m).with_text(t), mode, rules(tmux))
    }

    /// Press, repeat, release of one key; `text` accompanies press/repeat (egui sends no
    /// Text on release).
    fn prr(k: Key, m: Mods, text: Option<&str>, mode: TermMode) -> [Vec<u8>; 3] {
        [KeyAction::Press, KeyAction::Repeat, KeyAction::Release].map(|action| {
            let input = KeyInput {
                key: k,
                mods: m,
                text: if action == KeyAction::Release {
                    None
                } else {
                    text
                },
                action,
            };
            encode_key(&input, mode, KeyRules::Legacy)
        })
    }

    fn kitty(flags: u8) -> TermMode {
        let mut m = TermMode::empty();
        for (bit, f) in [
            TermMode::DISAMBIGUATE_ESC_CODES,
            TermMode::REPORT_EVENT_TYPES,
            TermMode::REPORT_ALTERNATE_KEYS,
            TermMode::REPORT_ALL_KEYS_AS_ESC,
            TermMode::REPORT_ASSOCIATED_TEXT,
        ]
        .into_iter()
        .enumerate()
        {
            if flags & (1 << bit) != 0 {
                m |= f;
            }
        }
        m
    }

    fn s(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    fn check_prr(flags: u8, k: Key, m: Mods, text: Option<&str>, want: [&str; 3]) {
        let got = prr(k, m, text, kitty(flags));
        assert_eq!(
            got.each_ref().map(|b| s(b)),
            want.map(str::to_owned),
            "flags {flags} key {k:?} mods {m:?}"
        );
    }

    #[test]
    fn kitty_flags_2_event_types_only() {
        let a = Key::Char('a');
        check_prr(2, a, Mods::NONE, Some("a"), ["a", "a", "\x1b[97;1:3u"]);
        check_prr(2, a, Mods::SHIFT, Some("A"), ["A", "A", "\x1b[97;2:3u"]);
        check_prr(
            2,
            a,
            Mods::CTRL,
            None,
            ["\x01", "\x1b[97;5:2u", "\x1b[97;5:3u"],
        );
        check_prr(2, Key::Enter, Mods::NONE, None, ["\r", "\r", ""]);
        check_prr(
            2,
            Key::Enter,
            Mods::SHIFT,
            None,
            ["\x1b[13;2u", "\x1b[13;2:2u", "\x1b[13;2:3u"],
        );
        check_prr(2, Key::Escape, Mods::NONE, None, ["\x1b", "\x1b", ""]);
        check_prr(2, Key::Tab, Mods::NONE, None, ["\t", "\t", ""]);
        check_prr(2, Key::Backspace, Mods::NONE, None, ["\x7f", "\x7f", ""]);
    }

    #[test]
    fn kitty_flags_7_disambiguate_events_alternates() {
        let a = Key::Char('a');
        check_prr(7, a, Mods::NONE, Some("a"), ["a", "a", "\x1b[97;1:3u"]);
        check_prr(7, a, Mods::SHIFT, Some("A"), ["A", "A", "\x1b[97:65;2:3u"]);
        check_prr(
            7,
            a,
            Mods::CTRL,
            None,
            ["\x1b[97;5u", "\x1b[97;5:2u", "\x1b[97;5:3u"],
        );
        check_prr(7, Key::Enter, Mods::NONE, None, ["\r", "\r", ""]);
        check_prr(
            7,
            Key::Escape,
            Mods::NONE,
            None,
            ["\x1b[27u", "\x1b[27;1:2u", "\x1b[27;1:3u"],
        );
    }

    #[test]
    fn kitty_flags_15_all_keys_as_escapes() {
        let a = Key::Char('a');
        check_prr(
            15,
            a,
            Mods::NONE,
            Some("a"),
            ["\x1b[97u", "\x1b[97;1:2u", "\x1b[97;1:3u"],
        );
        check_prr(
            15,
            a,
            Mods::SHIFT,
            Some("A"),
            ["\x1b[97:65;2u", "\x1b[97:65;2:2u", "\x1b[97:65;2:3u"],
        );
        check_prr(
            15,
            a,
            Mods::CTRL,
            None,
            ["\x1b[97;5u", "\x1b[97;5:2u", "\x1b[97;5:3u"],
        );
        check_prr(
            15,
            Key::Enter,
            Mods::NONE,
            None,
            ["\x1b[13u", "\x1b[13;1:2u", "\x1b[13;1:3u"],
        );
        check_prr(
            15,
            Key::Escape,
            Mods::NONE,
            None,
            ["\x1b[27u", "\x1b[27;1:2u", "\x1b[27;1:3u"],
        );
    }

    #[test]
    fn kitty_flags_31_associated_text() {
        let a = Key::Char('a');
        check_prr(
            31,
            a,
            Mods::NONE,
            Some("a"),
            ["\x1b[97;;97u", "\x1b[97;1:2;97u", "\x1b[97;1:3u"],
        );
        check_prr(
            31,
            a,
            Mods::SHIFT,
            Some("A"),
            ["\x1b[97:65;2;65u", "\x1b[97:65;2:2;65u", "\x1b[97:65;2:3u"],
        );
        check_prr(
            31,
            a,
            Mods::CTRL,
            None,
            ["\x1b[97;5u", "\x1b[97;5:2u", "\x1b[97;5:3u"],
        );
        check_prr(
            31,
            Key::Enter,
            Mods::NONE,
            None,
            ["\x1b[13u", "\x1b[13;1:2u", "\x1b[13;1:3u"],
        );
        check_prr(
            31,
            Key::Escape,
            Mods::NONE,
            None,
            ["\x1b[27u", "\x1b[27;1:2u", "\x1b[27;1:3u"],
        );
    }

    #[test]
    fn modify_other_keys_direct_mode() {
        let mok = |k, m| encode_key(&KeyInput::press(k, m), LEGACY, KeyRules::ModifyOtherKeys);
        assert_eq!(mok(Key::Enter, Mods::SHIFT), b"\x1b[27;2;13~");
        assert_eq!(mok(Key::Enter, Mods::CTRL), b"\x1b[27;5;13~");
        assert_eq!(mok(Key::Enter, Mods::ALT), b"\x1b\r");
        assert_eq!(mok(Key::Enter, Mods::NONE), b"\r");
        // No tmux: a lone Escape is a plain ESC.
        assert_eq!(mok(Key::Escape, Mods::NONE), b"\x1b");
        assert_eq!(mok(Key::Escape, Mods::SHIFT), b"\x1b[27;2;27~");
        assert_eq!(mok(Key::Char('a'), Mods::CTRL), b"\x01");
        assert_eq!(mok(Key::Char('a'), SC), b"\x1b[27;6;97~");
        // Kitty flags take precedence over modifyOtherKeys.
        assert_eq!(
            encode_key(
                &KeyInput::press(Key::Enter, Mods::SHIFT),
                kitty(1),
                KeyRules::ModifyOtherKeys
            ),
            b"\x1b[13;2u"
        );
    }

    const SC: Mods = Mods::new(true, false, true);
    const SA: Mods = Mods::new(true, true, false);
    const AC: Mods = Mods::new(false, true, true);
    const SAC: Mods = Mods::new(true, true, true);

    #[test]
    fn mods_param() {
        assert_eq!(Mods::NONE.param(), 1);
        assert_eq!(Mods::SHIFT.param(), 2);
        assert_eq!(Mods::ALT.param(), 3);
        assert_eq!(SA.param(), 4);
        assert_eq!(Mods::CTRL.param(), 5);
        assert_eq!(SC.param(), 6);
        assert_eq!(AC.param(), 7);
        assert_eq!(SAC.param(), 8);
    }

    #[test]
    fn ctrl_cxv_raw_bytes_all_modes() {
        for tmux in [false, true] {
            assert_eq!(key(Key::Char('c'), Mods::CTRL, LEGACY, tmux), b"\x03");
            assert_eq!(key(Key::Char('x'), Mods::CTRL, LEGACY, tmux), b"\x18");
            assert_eq!(key(Key::Char('v'), Mods::CTRL, LEGACY, tmux), b"\x16");
            assert_eq!(key(Key::Char('a'), Mods::CTRL, LEGACY, tmux), b"\x01");
            assert_eq!(key(Key::Char('z'), Mods::CTRL, LEGACY, tmux), b"\x1a");
        }
        // Even when app cursor / bracketed paste are on.
        let m = TermMode::APP_CURSOR | TermMode::BRACKETED_PASTE;
        assert_eq!(key(Key::Char('v'), Mods::CTRL, m, true), b"\x16");
    }

    #[test]
    fn enter_variants_legacy() {
        assert_eq!(key(Key::Enter, Mods::NONE, LEGACY, false), b"\r");
        assert_eq!(key(Key::Enter, Mods::SHIFT, LEGACY, false), b"\r");
        assert_eq!(key(Key::Enter, Mods::CTRL, LEGACY, false), b"\r");
        assert_eq!(key(Key::Enter, Mods::ALT, LEGACY, false), b"\x1b\r");
        assert_eq!(
            key(Key::Enter, Mods::NONE, TermMode::LINE_FEED_NEW_LINE, false),
            b"\r\n"
        );
    }

    #[test]
    fn enter_variants_tmux() {
        assert_eq!(key(Key::Enter, Mods::NONE, LEGACY, true), b"\r");
        assert_eq!(key(Key::Enter, Mods::SHIFT, LEGACY, true), b"\x1b[27;2;13~");
        assert_eq!(key(Key::Enter, Mods::CTRL, LEGACY, true), b"\x1b[27;5;13~");
        assert_eq!(key(Key::Enter, Mods::ALT, LEGACY, true), b"\x1b\r");
        assert_eq!(key(Key::Enter, SC, LEGACY, true), b"\x1b[27;6;13~");
        assert_eq!(key(Key::Enter, SA, LEGACY, true), b"\x1b[27;4;13~");
        assert_eq!(key(Key::Enter, SAC, LEGACY, true), b"\x1b[27;8;13~");
    }

    #[test]
    fn enter_variants_kitty() {
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        for tmux in [false, true] {
            assert_eq!(key(Key::Enter, Mods::NONE, k, tmux), b"\r");
            assert_eq!(key(Key::Enter, Mods::SHIFT, k, tmux), b"\x1b[13;2u");
            assert_eq!(key(Key::Enter, Mods::CTRL, k, tmux), b"\x1b[13;5u");
            assert_eq!(key(Key::Enter, Mods::ALT, k, tmux), b"\x1b[13;3u");
        }
        let all = k | TermMode::REPORT_ALL_KEYS_AS_ESC;
        assert_eq!(key(Key::Enter, Mods::NONE, all, false), b"\x1b[13u");
    }

    #[test]
    fn escape_forms() {
        assert_eq!(key(Key::Escape, Mods::NONE, LEGACY, false), b"\x1b");
        assert_eq!(key(Key::Escape, Mods::ALT, LEGACY, false), b"\x1b\x1b");
        assert_eq!(key(Key::Escape, Mods::NONE, LEGACY, true), b"\x1bO[");
        assert_eq!(key(Key::Escape, Mods::ALT, LEGACY, true), b"\x1b\x1b");
        assert_eq!(
            key(Key::Escape, Mods::SHIFT, LEGACY, true),
            b"\x1b[27;2;27~"
        );
        assert_eq!(key(Key::Escape, Mods::CTRL, LEGACY, true), b"\x1b[27;5;27~");
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(key(Key::Escape, Mods::NONE, k, true), b"\x1b[27u");
        assert_eq!(key(Key::Escape, Mods::SHIFT, k, false), b"\x1b[27;2u");
        // Only event-type flag (no disambiguate): Esc stays legacy.
        assert_eq!(
            key(Key::Escape, Mods::NONE, TermMode::REPORT_EVENT_TYPES, false),
            b"\x1b"
        );
    }

    #[test]
    fn tab_and_backspace() {
        assert_eq!(key(Key::Tab, Mods::NONE, LEGACY, false), b"\t");
        assert_eq!(key(Key::Tab, Mods::SHIFT, LEGACY, false), b"\x1b[Z");
        assert_eq!(key(Key::Tab, Mods::ALT, LEGACY, false), b"\x1b\t");
        assert_eq!(key(Key::Tab, Mods::SHIFT, LEGACY, true), b"\x1b[Z");
        assert_eq!(key(Key::Tab, Mods::CTRL, LEGACY, true), b"\x1b[27;5;9~");
        assert_eq!(key(Key::Tab, SC, LEGACY, true), b"\x1b[27;6;9~");
        assert_eq!(key(Key::Backspace, Mods::NONE, LEGACY, false), b"\x7f");
        assert_eq!(key(Key::Backspace, Mods::CTRL, LEGACY, false), b"\x08");
        assert_eq!(key(Key::Backspace, Mods::ALT, LEGACY, false), b"\x1b\x7f");
        assert_eq!(key(Key::Backspace, AC, LEGACY, false), b"\x1b\x08");
        assert_eq!(key(Key::Backspace, Mods::NONE, LEGACY, true), b"\x7f");
        assert_eq!(key(Key::Backspace, Mods::ALT, LEGACY, true), b"\x1b\x7f");
        assert_eq!(
            key(Key::Backspace, Mods::CTRL, LEGACY, true),
            b"\x1b[27;5;127~"
        );
        assert_eq!(
            key(Key::Backspace, Mods::SHIFT, LEGACY, true),
            b"\x1b[27;2;127~"
        );
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(key(Key::Tab, Mods::NONE, k, false), b"\t");
        assert_eq!(key(Key::Tab, Mods::SHIFT, k, false), b"\x1b[9;2u");
        assert_eq!(key(Key::Backspace, Mods::NONE, k, false), b"\x7f");
        assert_eq!(key(Key::Backspace, Mods::CTRL, k, false), b"\x1b[127;5u");
    }

    #[test]
    fn printable_text_and_alt() {
        for tmux in [false, true] {
            assert_eq!(txt(Key::Char('a'), Mods::NONE, "a", LEGACY, tmux), b"a");
            assert_eq!(txt(Key::Char('a'), Mods::SHIFT, "A", LEGACY, tmux), b"A");
            assert_eq!(txt(Key::Char('a'), Mods::ALT, "a", LEGACY, tmux), b"\x1ba");
            assert_eq!(txt(Key::Char('a'), SA, "A", LEGACY, tmux), b"\x1bA");
            // AltGr-produced text (no alt/ctrl in egui modifiers) is sent verbatim.
            assert_eq!(txt(Key::Char('q'), Mods::NONE, "@", LEGACY, tmux), b"@");
            assert_eq!(
                txt(Key::Char('e'), Mods::NONE, "€", LEGACY, tmux),
                "€".as_bytes()
            );
            assert_eq!(txt(Key::Char(' '), Mods::NONE, " ", LEGACY, tmux), b" ");
            // Missing text falls back to the key.
            assert_eq!(key(Key::Char('b'), Mods::ALT, LEGACY, tmux), b"\x1bb");
        }
        assert_eq!(encode_text("漢字"), "漢字".as_bytes());
        // Control characters are never treated as text.
        assert_eq!(
            txt(Key::Char('a'), Mods::NONE, "\u{7}", LEGACY, false),
            b"a"
        );
    }

    #[test]
    fn ctrl_combos_legacy() {
        assert_eq!(key(Key::Char('a'), SC, LEGACY, false), b"\x01");
        assert_eq!(key(Key::Char('c'), AC, LEGACY, false), b"\x1b\x03");
        assert_eq!(key(Key::Char(' '), Mods::CTRL, LEGACY, false), b"\x00");
        assert_eq!(key(Key::Char('['), Mods::CTRL, LEGACY, false), b"\x1b");
        assert_eq!(key(Key::Char('\\'), Mods::CTRL, LEGACY, false), b"\x1c");
        assert_eq!(key(Key::Char(']'), Mods::CTRL, LEGACY, false), b"\x1d");
        assert_eq!(key(Key::Char('/'), Mods::CTRL, LEGACY, false), b"\x1f");
        assert_eq!(key(Key::Char('2'), Mods::CTRL, LEGACY, false), b"\x00");
        assert_eq!(key(Key::Char('8'), Mods::CTRL, LEGACY, false), b"\x7f");
        assert_eq!(key(Key::Char('1'), Mods::CTRL, LEGACY, false), b"1");
        assert_eq!(key(Key::Char('.'), Mods::CTRL, LEGACY, false), b".");
    }

    #[test]
    fn ctrl_combos_tmux_modify_other_keys() {
        assert_eq!(key(Key::Char('a'), SC, LEGACY, true), b"\x1b[27;6;97~");
        assert_eq!(key(Key::Char('c'), AC, LEGACY, true), b"\x1b\x03");
        assert_eq!(key(Key::Char('c'), SAC, LEGACY, true), b"\x1b[27;8;99~");
        assert_eq!(key(Key::Char(' '), Mods::CTRL, LEGACY, true), b"\x00");
        assert_eq!(key(Key::Char('['), Mods::CTRL, LEGACY, true), b"\x1b");
        assert_eq!(
            key(Key::Char('1'), Mods::CTRL, LEGACY, true),
            b"\x1b[27;5;49~"
        );
        assert_eq!(
            key(Key::Char('2'), Mods::CTRL, LEGACY, true),
            b"\x1b[27;5;50~"
        );
        assert_eq!(
            key(Key::Char('.'), Mods::CTRL, LEGACY, true),
            b"\x1b[27;5;46~"
        );
        assert_eq!(
            key(Key::Char('/'), Mods::CTRL, LEGACY, true),
            b"\x1b[27;5;47~"
        );
        assert_eq!(
            key(Key::Char(';'), Mods::CTRL, LEGACY, true),
            b"\x1b[27;5;59~"
        );
        assert_eq!(
            txt(Key::Char(' '), Mods::SHIFT, " ", LEGACY, true),
            b"\x1b[27;2;32~"
        );
        assert_eq!(key(Key::Char(' '), SC, LEGACY, true), b"\x1b[27;6;32~");
        assert_eq!(txt(Key::Char(' '), Mods::ALT, " ", LEGACY, true), b"\x1b ");
        // Shift+letter is plain text, no modifyOtherKeys.
        assert_eq!(txt(Key::Char('a'), Mods::SHIFT, "A", LEGACY, true), b"A");
    }

    #[test]
    fn app_cursor_arrows_home_end() {
        let ac = TermMode::APP_CURSOR;
        for tmux in [false, true] {
            assert_eq!(key(Key::ArrowUp, Mods::NONE, LEGACY, tmux), b"\x1b[A");
            assert_eq!(key(Key::ArrowDown, Mods::NONE, LEGACY, tmux), b"\x1b[B");
            assert_eq!(key(Key::ArrowRight, Mods::NONE, LEGACY, tmux), b"\x1b[C");
            assert_eq!(key(Key::ArrowLeft, Mods::NONE, LEGACY, tmux), b"\x1b[D");
            assert_eq!(key(Key::ArrowUp, Mods::NONE, ac, tmux), b"\x1bOA");
            assert_eq!(key(Key::ArrowLeft, Mods::NONE, ac, tmux), b"\x1bOD");
            assert_eq!(key(Key::Home, Mods::NONE, LEGACY, tmux), b"\x1b[H");
            assert_eq!(key(Key::End, Mods::NONE, ac, tmux), b"\x1bOF");
            assert_eq!(key(Key::ArrowUp, Mods::SHIFT, ac, tmux), b"\x1b[1;2A");
            assert_eq!(key(Key::ArrowRight, Mods::CTRL, LEGACY, tmux), b"\x1b[1;5C");
            assert_eq!(key(Key::Home, SAC, ac, tmux), b"\x1b[1;8H");
        }
    }

    #[test]
    fn function_and_editing_keys() {
        for tmux in [false, true] {
            assert_eq!(key(Key::F(1), Mods::NONE, LEGACY, tmux), b"\x1bOP");
            assert_eq!(
                key(Key::F(4), Mods::NONE, TermMode::APP_CURSOR, tmux),
                b"\x1bOS"
            );
            assert_eq!(key(Key::F(1), Mods::SHIFT, LEGACY, tmux), b"\x1b[1;2P");
            assert_eq!(key(Key::F(3), Mods::CTRL, LEGACY, tmux), b"\x1b[1;5R");
            assert_eq!(key(Key::F(5), Mods::NONE, LEGACY, tmux), b"\x1b[15~");
            assert_eq!(key(Key::F(5), Mods::SHIFT, LEGACY, tmux), b"\x1b[15;2~");
            assert_eq!(key(Key::F(6), Mods::NONE, LEGACY, tmux), b"\x1b[17~");
            assert_eq!(key(Key::F(10), Mods::NONE, LEGACY, tmux), b"\x1b[21~");
            assert_eq!(key(Key::F(11), Mods::NONE, LEGACY, tmux), b"\x1b[23~");
            assert_eq!(key(Key::F(12), AC, LEGACY, tmux), b"\x1b[24;7~");
            assert_eq!(key(Key::F(13), Mods::NONE, LEGACY, tmux), b"\x1b[25~");
            assert_eq!(key(Key::F(20), Mods::NONE, LEGACY, tmux), b"\x1b[34~");
            assert_eq!(key(Key::F(21), Mods::NONE, LEGACY, tmux), b"");
            assert_eq!(key(Key::Insert, Mods::NONE, LEGACY, tmux), b"\x1b[2~");
            assert_eq!(key(Key::Delete, Mods::NONE, LEGACY, tmux), b"\x1b[3~");
            assert_eq!(key(Key::Delete, Mods::CTRL, LEGACY, tmux), b"\x1b[3;5~");
            assert_eq!(key(Key::PageUp, Mods::NONE, LEGACY, tmux), b"\x1b[5~");
            assert_eq!(key(Key::PageDown, Mods::SHIFT, LEGACY, tmux), b"\x1b[6;2~");
        }
    }

    #[test]
    fn kitty_disambiguate() {
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(txt(Key::Char('a'), Mods::NONE, "a", k, false), b"a");
        assert_eq!(txt(Key::Char('a'), Mods::SHIFT, "A", k, false), b"A");
        assert_eq!(txt(Key::Char('a'), Mods::ALT, "a", k, false), b"\x1b[97;3u");
        assert_eq!(key(Key::Char('c'), Mods::CTRL, k, false), b"\x1b[99;5u");
        assert_eq!(key(Key::Char('a'), SC, k, false), b"\x1b[97;6u");
        assert_eq!(key(Key::Char(' '), Mods::CTRL, k, false), b"\x1b[32;5u");
        assert_eq!(key(Key::Char('1'), Mods::CTRL, k, false), b"\x1b[49;5u");
        assert_eq!(key(Key::ArrowUp, Mods::NONE, k, false), b"\x1b[A");
        assert_eq!(key(Key::ArrowUp, Mods::CTRL, k, false), b"\x1b[1;5A");
        assert_eq!(key(Key::F(1), Mods::NONE, k, false), b"\x1b[P");
        assert_eq!(key(Key::F(3), Mods::NONE, k, false), b"\x1b[13~");
        assert_eq!(key(Key::F(5), Mods::ALT, k, false), b"\x1b[15;3~");
        assert_eq!(key(Key::Delete, Mods::NONE, k, false), b"\x1b[3~");
        assert_eq!(key(Key::F(13), Mods::NONE, k, false), b"\x1b[57376u");
        assert_eq!(key(Key::F(35), Mods::NONE, k, false), b"\x1b[57398u");
        // Release without REPORT_EVENT_TYPES: nothing.
        let rel = KeyInput {
            action: KeyAction::Release,
            ..KeyInput::press(Key::Char('a'), Mods::CTRL)
        };
        assert_eq!(encode_key(&rel, k, KeyRules::Legacy), b"");
        // Release in non-kitty modes: nothing.
        assert_eq!(encode_key(&rel, LEGACY, KeyRules::Tmux), b"");
    }

    #[test]
    fn kitty_event_types() {
        let k = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES;
        let mk = |key, mods, action| KeyInput {
            key,
            mods,
            text: None,
            action,
        };
        let encode_key =
            |input: &KeyInput<'_>, mode, _: bool| encode_key(input, mode, KeyRules::Legacy);
        assert_eq!(
            encode_key(&mk(Key::Char('a'), Mods::CTRL, KeyAction::Press), k, false),
            b"\x1b[97;5u"
        );
        assert_eq!(
            encode_key(&mk(Key::Char('a'), Mods::CTRL, KeyAction::Repeat), k, false),
            b"\x1b[97;5:2u"
        );
        assert_eq!(
            encode_key(
                &mk(Key::Char('a'), Mods::CTRL, KeyAction::Release),
                k,
                false
            ),
            b"\x1b[97;5:3u"
        );
        assert_eq!(
            encode_key(
                &mk(Key::Char('a'), Mods::NONE, KeyAction::Release),
                k,
                false
            ),
            b"\x1b[97;1:3u"
        );
        assert_eq!(
            encode_key(
                &mk(Key::ArrowLeft, Mods::NONE, KeyAction::Release),
                k,
                false
            ),
            b"\x1b[1;1:3D"
        );
        assert_eq!(
            encode_key(&mk(Key::PageUp, Mods::SHIFT, KeyAction::Repeat), k, false),
            b"\x1b[5;2:2~"
        );
        assert_eq!(
            encode_key(&mk(Key::Escape, Mods::NONE, KeyAction::Release), k, false),
            b"\x1b[27;1:3u"
        );
        // Enter/Tab/Backspace have no release events unless all keys are escape codes.
        assert_eq!(
            encode_key(&mk(Key::Enter, Mods::NONE, KeyAction::Release), k, false),
            b""
        );
        let all = k | TermMode::REPORT_ALL_KEYS_AS_ESC;
        assert_eq!(
            encode_key(&mk(Key::Enter, Mods::NONE, KeyAction::Release), all, false),
            b"\x1b[13;1:3u"
        );
        // Text keys still send text on press.
        let press = KeyInput::press(Key::Char('a'), Mods::NONE).with_text("a");
        assert_eq!(encode_key(&press, k, false), b"a");
    }

    #[test]
    fn kitty_all_keys_alternate_and_text() {
        let all = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC;
        assert_eq!(
            txt(Key::Char('a'), Mods::NONE, "a", all, false),
            b"\x1b[97u"
        );
        assert_eq!(
            txt(Key::Char('a'), Mods::SHIFT, "A", all, false),
            b"\x1b[97;2u"
        );
        assert_eq!(key(Key::Tab, Mods::NONE, all, false), b"\x1b[9u");
        assert_eq!(key(Key::Backspace, Mods::NONE, all, false), b"\x1b[127u");
        let alt = all | TermMode::REPORT_ALTERNATE_KEYS;
        assert_eq!(
            txt(Key::Char('a'), Mods::SHIFT, "A", alt, false),
            b"\x1b[97:65;2u"
        );
        assert_eq!(
            txt(Key::Char('1'), Mods::SHIFT, "!", alt, false),
            b"\x1b[49:33;2u"
        );
        assert_eq!(key(Key::Char('a'), SC, alt, false), b"\x1b[97:65;6u");
        assert_eq!(
            txt(Key::Char('a'), Mods::NONE, "a", alt, false),
            b"\x1b[97u"
        );
        let text = alt | TermMode::REPORT_ASSOCIATED_TEXT;
        assert_eq!(
            txt(Key::Char('a'), Mods::NONE, "a", text, false),
            b"\x1b[97;;97u"
        );
        assert_eq!(
            txt(Key::Char('a'), Mods::SHIFT, "A", text, false),
            b"\x1b[97:65;2;65u"
        );
        // Alternate keys without all-keys: shift+letter is still plain text.
        let alt_only = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALTERNATE_KEYS;
        assert_eq!(txt(Key::Char('a'), Mods::SHIFT, "A", alt_only, false), b"A");
        assert_eq!(key(Key::Char('a'), SC, alt_only, false), b"\x1b[97:65;6u");
    }

    #[test]
    fn mouse_sgr() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let n = Mods::NONE;
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 0, 0, m),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Release, n, 4, 6, m),
            b"\x1b[<0;5;7m"
        );
        assert_eq!(
            encode_mouse(MouseButton::Right, MouseAction::Press, n, 9, 9, m),
            b"\x1b[<2;10;10M"
        );
        assert_eq!(
            encode_mouse(MouseButton::Middle, MouseAction::Release, n, 9, 9, m),
            b"\x1b[<1;10;10m"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Motion, n, 2, 3, m),
            b"\x1b[<32;3;4M"
        );
        assert_eq!(
            encode_mouse(MouseButton::NoButton, MouseAction::Motion, n, 2, 3, m),
            b"\x1b[<35;3;4M"
        );
        assert_eq!(
            encode_mouse(MouseButton::WheelUp, MouseAction::Press, n, 9, 9, m),
            b"\x1b[<64;10;10M"
        );
        assert_eq!(
            encode_mouse(MouseButton::WheelDown, MouseAction::Press, n, 9, 9, m),
            b"\x1b[<65;10;10M"
        );
        assert_eq!(
            encode_mouse(
                MouseButton::WheelUp,
                MouseAction::Press,
                Mods::SHIFT,
                9,
                9,
                m
            ),
            b"\x1b[<68;10;10M"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, Mods::ALT, 0, 0, m),
            b"\x1b[<8;1;1M"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, Mods::CTRL, 0, 0, m),
            b"\x1b[<16;1;1M"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Motion, SAC, 0, 0, m),
            b"\x1b[<60;1;1M"
        );
        // SGR has no coordinate limit.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 299, 0, m),
            b"\x1b[<0;300;1M"
        );
    }

    #[test]
    fn mouse_x10_and_clamp() {
        let m = TermMode::MOUSE_REPORT_CLICK;
        let n = Mods::NONE;
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 0, 0, m),
            b"\x1b[M\x20\x21\x21"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Release, n, 0, 0, m),
            b"\x1b[M\x23\x21\x21"
        );
        assert_eq!(
            encode_mouse(MouseButton::WheelDown, MouseAction::Press, n, 1, 2, m),
            b"\x1b[M\x61\x22\x23"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Motion, Mods::CTRL, 0, 0, m),
            b"\x1b[M\x50\x21\x21"
        );
        // col 221 (1-based 222) → 254; col 222 (1-based 223) → 255; col 223+ clamps to 255.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 221, 0, m),
            b"\x1b[M\x20\xfe\x21"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 222, 0, m),
            b"\x1b[M\x20\xff\x21"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 223, 0, m),
            b"\x1b[M\x20\xff\x21"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 5000, 400, m),
            b"\x1b[M\x20\xff\xff"
        );
    }

    #[test]
    fn mouse_utf8() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        let n = Mods::NONE;
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 0, 0, m),
            b"\x1b[M\x20\x21\x21"
        );
        // 1-based 300 → 332 = U+014C → C5 8C.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 299, 0, m),
            b"\x1b[M\x20\xc5\x8c\x21"
        );
        // 1-based 95 → 127 is still one byte; 96 → 128 = C2 80.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 94, 0, m),
            b"\x1b[M\x20\x7f\x21"
        );
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 95, 0, m),
            b"\x1b[M\x20\xc2\x80\x21"
        );
        // Clamp at 2015 → 2047 = U+07FF → DF BF.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, n, 9000, 0, m),
            b"\x1b[M\x20\xdf\xbf\x21"
        );
    }

    #[test]
    fn mouse_wanted() {
        let click = TermMode::MOUSE_REPORT_CLICK;
        let drag = TermMode::MOUSE_DRAG;
        let motion = TermMode::MOUSE_MOTION;
        assert!(!mouse_report_wanted(MouseAction::Press, false, LEGACY));
        assert!(mouse_report_wanted(MouseAction::Press, false, click));
        assert!(mouse_report_wanted(MouseAction::Release, true, drag));
        assert!(!mouse_report_wanted(MouseAction::Motion, true, click));
        assert!(mouse_report_wanted(MouseAction::Motion, true, drag));
        assert!(!mouse_report_wanted(MouseAction::Motion, false, drag));
        assert!(mouse_report_wanted(MouseAction::Motion, false, motion));
        assert!(mouse_report_wanted(MouseAction::Motion, true, motion));
    }

    #[test]
    fn paste_bracketed_and_plain() {
        let b = TermMode::BRACKETED_PASTE;
        assert_eq!(encode_paste("hi\nthere", b), b"\x1b[200~hi\nthere\x1b[201~");
        assert_eq!(encode_paste("a\x1b[201~b", b), b"\x1b[200~ab\x1b[201~");
        assert_eq!(
            encode_paste("x\x1b[20\x1b[201~1~y", b),
            b"\x1b[200~xy\x1b[201~"
        );
        assert_eq!(encode_paste("", b), b"\x1b[200~\x1b[201~");
        assert_eq!(encode_paste("a\nb\r\nc\rd", LEGACY), b"a\rb\rc\rd");
        assert_eq!(encode_paste("\n\n", LEGACY), b"\r\r");
        assert_eq!(encode_paste("tail\r", LEGACY), b"tail\r");
        assert_eq!(encode_paste("漢\n", LEGACY), "漢\r".as_bytes());
    }

    #[test]
    fn focus_reports() {
        assert_eq!(encode_focus(true, LEGACY), None);
        assert_eq!(
            encode_focus(true, TermMode::FOCUS_IN_OUT),
            Some(&b"\x1b[I"[..])
        );
        assert_eq!(
            encode_focus(false, TermMode::FOCUS_IN_OUT),
            Some(&b"\x1b[O"[..])
        );
    }

    #[test]
    fn alternate_scroll() {
        assert_eq!(encode_alt_scroll(3, LEGACY), b"\x1b[A\x1b[A\x1b[A");
        assert_eq!(encode_alt_scroll(-1, TermMode::APP_CURSOR), b"\x1bOB");
        assert_eq!(encode_alt_scroll(0, LEGACY), b"");
        assert!(wants_alt_scroll(
            TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL
        ));
        assert!(!wants_alt_scroll(TermMode::ALT_SCREEN));
        assert!(!wants_alt_scroll(
            TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL | TermMode::MOUSE_REPORT_CLICK
        ));
    }
}

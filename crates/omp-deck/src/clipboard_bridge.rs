//! egui has no system-clipboard backend in this build (see docs/notes/egui-input.md), so plain
//! Ctrl+C/X/V in egui widgets are bridged to the shared `deck_term::Clipboard` by hand.

use deck_term::Clipboard;
use egui::{Event, Key, Modifiers, OutputCommand};

fn is_plain_ctrl(m: &Modifiers) -> bool {
    m.ctrl && !m.shift && !m.alt && !m.mac_cmd
}

fn clipboard_key(ev: &Event) -> Option<Key> {
    match ev {
        Event::Key {
            key: key @ (Key::C | Key::X | Key::V),
            pressed: true,
            modifiers,
            ..
        } if is_plain_ctrl(modifiers) => Some(*key),
        _ => None,
    }
}

/// Rewrite plain Ctrl+C/X/V key presses into `Event::Copy/Cut/Paste`. `paste` is only called
/// when a Ctrl+V is present; `None` (empty clipboard) drops the key, like egui-winit does.
pub fn rewrite_events(events: &mut Vec<Event>, mut paste: impl FnMut() -> Option<String>) {
    if !events.iter().any(|e| clipboard_key(e).is_some()) {
        return;
    }
    let old = std::mem::take(events);
    events.reserve(old.len());
    for ev in old {
        match clipboard_key(&ev) {
            Some(Key::C) => events.push(Event::Copy),
            Some(Key::X) => events.push(Event::Cut),
            Some(_) => {
                if let Some(text) = paste().filter(|t| !t.is_empty()) {
                    events.push(Event::Paste(text.replace("\r\n", "\n")));
                }
            }
            None => events.push(ev),
        }
    }
}

/// Call from `App::raw_input_hook` only while no terminal has focus.
pub fn rewrite_raw_input(raw: &mut egui::RawInput, clipboard: &Clipboard) {
    rewrite_events(&mut raw.events, || clipboard.get_text());
}

/// After each frame: widget copies (`OutputCommand::CopyText`) go to the system clipboard.
pub fn forward_copies(ctx: &egui::Context, clipboard: &Clipboard) {
    ctx.output(|o| {
        for cmd in &o.commands {
            if let OutputCommand::CopyText(text) = cmd {
                clipboard.set_text(text);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key, m: Modifiers) -> Event {
        Event::Key {
            key: k,
            physical_key: Some(k),
            pressed: true,
            repeat: false,
            modifiers: m,
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        }
    }

    #[test]
    fn rewrites_plain_ctrl_only() {
        let mut ev = vec![
            key(Key::C, ctrl()),
            key(Key::X, ctrl()),
            key(Key::V, ctrl()),
            key(
                Key::V,
                Modifiers {
                    shift: true,
                    ..ctrl()
                },
            ),
            key(Key::A, ctrl()),
        ];
        rewrite_events(&mut ev, || Some("a\r\nb".into()));
        assert_eq!(ev[0], Event::Copy);
        assert_eq!(ev[1], Event::Cut);
        assert_eq!(ev[2], Event::Paste("a\nb".into()));
        assert!(matches!(ev[3], Event::Key { key: Key::V, .. }));
        assert!(matches!(ev[4], Event::Key { key: Key::A, .. }));
        assert_eq!(ev.len(), 5);
    }

    #[test]
    fn empty_clipboard_drops_paste() {
        let mut ev = vec![key(Key::V, ctrl())];
        rewrite_events(&mut ev, || None);
        assert!(ev.is_empty());
    }

    #[test]
    fn untouched_without_clipboard_keys() {
        let mut ev = vec![Event::Text("x".into())];
        rewrite_events(&mut ev, || panic!("must not read clipboard"));
        assert_eq!(ev, vec![Event::Text("x".into())]);
    }
}

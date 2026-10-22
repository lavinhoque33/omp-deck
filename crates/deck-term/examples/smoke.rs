//! Visual smoke test for the terminal widget.
//!
//! `cargo run -p deck-term --example smoke` opens a window running `$SHELL`.
//! `cargo run -p deck-term --example smoke -- --screenshot /tmp/out.png` prints a test pattern
//! (truecolor, 256 colors, SGR attributes, CJK, emoji, box drawing, braille, powerline, OSC 8),
//! saves a screenshot as PNG and exits.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use deck_term::{SpawnSpec, TermOptions, Terminal, TerminalView, install_fonts};

fn pattern() -> String {
    let mut s = String::from("\x1b]0;deck-term smoke\x07\x1b[1mdeck-term smoke\x1b[0m\r\n");
    for i in 0..96 {
        let r = 255 - i * 255 / 95;
        let g = (i * 510 / 95).min(510);
        let g = if g > 255 { 510 - g } else { g };
        let b = i * 255 / 95;
        s.push_str(&format!("\x1b[48;2;{r};{g};{b}m "));
    }
    s.push_str("\x1b[0m\r\n");
    for row in 0..8 {
        for col in 0..32 {
            let n = row * 32 + col;
            s.push_str(&format!("\x1b[48;5;{n}m  "));
        }
        s.push_str("\x1b[0m\r\n");
    }
    s.push_str(
        "\x1b[1mbold\x1b[0m \x1b[2mdim\x1b[0m \x1b[3mitalic\x1b[0m \x1b[1;3mbolditalic\x1b[0m \
         \x1b[4munderline\x1b[0m \x1b[4:2mdouble\x1b[0m \x1b[4:3;58:2::255:0:0mcurly-red\x1b[0m \
         \x1b[4:4mdotted\x1b[0m \x1b[4:5mdashed\x1b[0m \x1b[9mstrike\x1b[0m \x1b[7minverse\x1b[0m \
         [\x1b[8mhidden\x1b[0m]\r\n",
    );
    s.push_str("\x1b[31mred \x1b[32mgreen \x1b[33myellow \x1b[34mblue \x1b[35mmagenta \x1b[36mcyan \x1b[91mbright-red \x1b[38;2;255;128;0mtruecolor-orange\x1b[0m\r\n");
    s.push_str(
        "CJK: 漢字かな한글 | emoji: 🙂🚀🐦🔣🗂️ ⠙⠴ | combining: e\u{301}a\u{308} | ✓ ✗ ★\r\n",
    );
    s.push_str("╭──────╮ ┌──┬──┐ ╔══╦══╗ ┏━━┳━━┓ ┄┄┅┅ ╌╌\r\n");
    s.push_str("│ box  │ ├──┼──┤ ╠══╬══╣ ┣━━╋━━┫ ┆┇ ╎╏\r\n");
    s.push_str("╰──────╯ └──┴──┘ ╚══╩══╝ ┗━━┻━━┛ ╱╲╳ ╴╵╶╷\r\n");
    s.push_str("blocks: ▀▄█▌▐░▒▓▖▗▘▝▙▟ ▁▂▃▄▅▆▇█ | braille: ⣿⠁⡇⢸⠿ | powerline: \u{e0b0}\u{e0b1}\u{e0b2}\u{e0b3}\r\n");
    s.push_str("\x1b[44;97m\u{e0b0} status \x1b[0;34m\u{e0b0}\x1b[0m\r\n");
    s.push_str("link: \x1b]8;;https://example.com\x1b\\example.com (Ctrl+click)\x1b]8;;\x1b\\\r\n");
    s.push_str("\x1b[5 qprompt$ ");
    s
}

struct Smoke {
    term: Terminal,
    screenshot: Option<PathBuf>,
    frames: u32,
    requested: bool,
    done: Arc<AtomicBool>,
}

impl eframe::App for Smoke {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let resp = TerminalView::new(&mut self.term).font_size(15.0).show(ui);
                if self.frames == 1 {
                    resp.response.request_focus();
                }
                if let Some(title) = resp.events.title_changed {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
                }
                if let Some(url) = resp.open_url {
                    eprintln!("open_url: {url}");
                }
            });
        self.frames += 1;
        if let Some(path) = self.screenshot.clone() {
            if self.done.load(Ordering::SeqCst) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if self.frames > 30 && !self.requested {
                self.requested = true;
                let done = self.done.clone();
                let repaint = ctx.clone();
                ctx.request_screenshot(move |img| {
                    if let Err(e) = write_png(&path, img.size, &img.pixels) {
                        eprintln!("screenshot failed: {e}");
                    }
                    done.store(true, Ordering::SeqCst);
                    repaint.request_repaint();
                });
            }
            // Keep frames coming until the pattern settled and the screenshot is taken.
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
    }
}

/// Minimal PNG writer (RGBA8, stored deflate blocks).
fn write_png(
    path: &std::path::Path,
    size: [usize; 2],
    pixels: &[egui::Color32],
) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let [w, h] = size;
    let mut raw = Vec::with_capacity((w * 4 + 1) * h);
    for row in pixels.chunks(w) {
        raw.push(0);
        for p in row {
            raw.extend_from_slice(&p.to_srgba_unmultiplied());
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    let screenshot = args
        .iter()
        .position(|a| a == "--screenshot")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("deck-term smoke")
            .with_inner_size([1200.0, 720.0]),
        clipboard_shortcuts: false,
        ..Default::default()
    };
    eframe::run_native(
        "deck-term-smoke",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            let report = install_fonts(&cc.egui_ctx);
            eprintln!(
                "fonts: primary {:?}, {} loaded, missing {:?}",
                report.primary,
                report.loaded.len(),
                report.missing
            );
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let env = vec![
                ("TERM".into(), "xterm-256color".into()),
                ("COLORTERM".into(), "truecolor".into()),
            ];
            let spec = if screenshot.is_some() {
                SpawnSpec {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        "printf '%s' \"$1\"; sleep 60".into(),
                        "sh".into(),
                        pattern(),
                    ],
                    cwd,
                    env,
                    tmux_client: false,
                }
            } else {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                SpawnSpec {
                    program: shell,
                    args: Vec::new(),
                    cwd,
                    env,
                    tmux_client: false,
                }
            };
            let term = Terminal::spawn(spec, TermOptions::default(), cc.egui_ctx.clone())?;
            Ok(Box::new(Smoke {
                term,
                screenshot,
                frames: 0,
                requested: false,
                done: Arc::new(AtomicBool::new(false)),
            }))
        }),
    )
}

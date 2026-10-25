//! Kitty graphics protocol, the subset omp uses for inline images: direct (base64) transmission
//! of PNG / raw RGB(A) images, virtual placements (`U=1`) and deletes; images show through
//! Unicode placeholder cells (U+10EEEE + row/column diacritics, image id in the foreground
//! color, placement id in the underline color). Not supported: file/shm mediums, direct
//! placements, animation. Replies are never sent: omp always passes `q=2` (suppress all).
//!
//! The PTY thread recognises the APC strings ([`ApcScanner`]) and applies them to the
//! per-terminal [`Graphics`] store; images decode on a worker thread; the UI paints placeholder
//! cells from the store (render.rs).

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use alacritty_terminal::vte::ansi::Color;
use egui::epaint::{Pos2, Rect, pos2, vec2};
use egui::{ColorImage, TextureHandle, TextureId, TextureOptions};

/// Base char of a placeholder cell.
pub(crate) const PLACEHOLDER: char = '\u{10EEEE}';

/// Largest base64 payload of one image (all chunks).
const MAX_ENCODED: usize = 64 << 20;
/// Largest source image (pixels) accepted for decoding.
const MAX_SOURCE_PIXELS: u64 = 1 << 25;
/// Images larger than this (or the GPU's limit) on either side are downscaled.
const MAX_SIDE: usize = 4096;
/// Decoded bytes kept per terminal; least recently painted images are evicted beyond it.
const MAX_DECODED_TOTAL: usize = 256 << 20;
/// Control data before the `;` is short; anything longer is not a valid command.
const MAX_CONTROL: usize = 1024;

/// Row/column diacritics (kitty's `rowcolumn-diacritics.txt`): index = row/column number.
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// Row/column number encoded by a diacritic.
fn diacritic(c: char) -> Option<u32> {
    DIACRITICS
        .binary_search(&u32::from(c))
        .ok()
        .map(|i| i as u32)
}

/// A complete image upload: base64 `data` (all chunks), not yet decoded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Upload {
    pub id: u32,
    /// `f`: 24 (RGB), 32 (RGBA) or 100 (PNG).
    pub format: u32,
    /// `s`, `v`: pixel size of raw formats.
    pub width: u32,
    pub height: u32,
    /// `o=z`: zlib-compressed.
    pub zlib: bool,
    pub data: Vec<u8>,
    /// `a=T,U=1`: virtual placement (pid, cols, rows) created with the upload.
    pub place: Option<(u32, u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Transmit(Upload),
    /// Virtual placement (`a=p,U=1`); 0 cols/rows = derived from the image size.
    Place {
        id: u32,
        pid: u32,
        cols: u32,
        rows: u32,
    },
    /// `a=d`: `what` is the `d` key (default `a`).
    Delete {
        what: u8,
        id: u32,
        pid: u32,
    },
}

/// Parsed control keys of one APC command.
#[derive(Default)]
struct Controls {
    a: Option<u8>,
    d: Option<u8>,
    t: Option<u8>,
    o: Option<u8>,
    f: u32,
    i: Option<u32>,
    p: u32,
    c: u32,
    r: u32,
    m: u32,
    s: u32,
    v: u32,
    u: u32,
}

impl Controls {
    fn parse(control: &[u8]) -> Controls {
        let mut k = Controls {
            f: 32,
            ..Controls::default()
        };
        for kv in control.split(|&b| b == b',') {
            let Some((&key, value)) = kv.split_first() else {
                continue;
            };
            let Some(value) = value.strip_prefix(b"=") else {
                continue;
            };
            let num = || {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
            };
            let ch = value.first().copied();
            match key {
                b'a' => k.a = ch,
                b'd' => k.d = ch,
                b't' => k.t = ch,
                b'o' => k.o = ch,
                b'f' => k.f = num().unwrap_or(0),
                b'i' => k.i = num(),
                b'p' => k.p = num().unwrap_or(0),
                b'c' => k.c = num().unwrap_or(0),
                b'r' => k.r = num().unwrap_or(0),
                b'm' => k.m = num().unwrap_or(0),
                b's' => k.s = num().unwrap_or(0),
                b'v' => k.v = num().unwrap_or(0),
                b'U' => k.u = num().unwrap_or(0),
                // q (replies are never sent), x/y/w/h/X/Y/z/C/I/S/O: unused.
                _ => {}
            }
        }
        k
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ApcState {
    #[default]
    Ground,
    Esc,
    /// After `ESC _`.
    Apc,
    /// After `ESC _ G`: collecting the body.
    Body,
    /// ESC inside the body: `\` ends it.
    BodyEsc,
}

/// Byte-level recogniser for kitty graphics APC strings (`ESC _ G <control> ; <payload> ESC \`).
/// Keeps its state (and a chunked upload) between reads.
pub(crate) struct ApcScanner {
    state: ApcState,
    body: Vec<u8>,
    /// The body exceeded the limit: the command is dropped.
    overflow: bool,
    /// Chunked upload in progress (`m=1`).
    upload: Option<Upload>,
    max_encoded: usize,
}

impl Default for ApcScanner {
    fn default() -> Self {
        ApcScanner {
            state: ApcState::Ground,
            body: Vec::new(),
            overflow: false,
            upload: None,
            max_encoded: MAX_ENCODED,
        }
    }
}

impl ApcScanner {
    /// Feeds child output; `on` receives every completed command.
    pub(crate) fn feed(&mut self, bytes: &[u8], mut on: impl FnMut(Command)) {
        const ESC: u8 = 0x1b;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match self.state {
                ApcState::Ground => match bytes[i..].iter().position(|&c| c == ESC) {
                    Some(p) => {
                        i += p + 1;
                        self.state = ApcState::Esc;
                        continue;
                    }
                    None => return,
                },
                ApcState::Esc => {
                    self.state = match b {
                        b'_' => ApcState::Apc,
                        ESC => ApcState::Esc,
                        _ => ApcState::Ground,
                    }
                }
                ApcState::Apc => {
                    self.state = match b {
                        b'G' => {
                            self.body.clear();
                            self.overflow = false;
                            ApcState::Body
                        }
                        ESC => ApcState::Esc,
                        _ => ApcState::Ground,
                    }
                }
                ApcState::Body => {
                    let rest = &bytes[i..];
                    let end = rest.iter().position(|&c| c == ESC);
                    self.push(&rest[..end.unwrap_or(rest.len())]);
                    match end {
                        Some(p) => {
                            i += p + 1;
                            self.state = ApcState::BodyEsc;
                            continue;
                        }
                        None => return,
                    }
                }
                ApcState::BodyEsc => {
                    if b == b'\\' {
                        self.state = ApcState::Ground;
                        self.finish(&mut on);
                    } else {
                        // Unterminated string: the ESC starts a new sequence.
                        self.abort_upload();
                        self.state = ApcState::Esc;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.overflow || self.body.len() + bytes.len() > self.max_encoded + MAX_CONTROL {
            self.overflow = true;
        } else {
            self.body.extend_from_slice(bytes);
        }
    }

    fn abort_upload(&mut self) {
        if let Some(up) = self.upload.take() {
            log::warn!("kitty graphics: upload of image {} aborted", up.id);
        }
    }

    fn finish(&mut self, on: &mut impl FnMut(Command)) {
        if self.overflow {
            log::warn!("kitty graphics: command over the size limit dropped");
            self.abort_upload();
        } else {
            let body = std::mem::take(&mut self.body);
            self.command(&body, on);
            self.body = body;
        }
        self.body.clear();
        if self.body.capacity() > 1 << 16 {
            self.body = Vec::new();
        }
    }

    fn command(&mut self, body: &[u8], on: &mut impl FnMut(Command)) {
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(p) => (&body[..p], &body[p + 1..]),
            None => (body, &[][..]),
        };
        if control.len() > MAX_CONTROL {
            return;
        }
        let k = Controls::parse(control);
        // Continuation chunk: only `m` (and `q`) keys.
        if k.a.is_none()
            && k.i.is_none()
            && let Some(mut up) = self.upload.take()
        {
            if up.data.len() + payload.len() > self.max_encoded {
                log::warn!(
                    "kitty graphics: image {} over the size limit dropped",
                    up.id
                );
                return;
            }
            up.data.extend_from_slice(payload);
            if k.m == 1 {
                self.upload = Some(up);
            } else {
                on(Command::Transmit(up));
            }
            return;
        }
        self.abort_upload();
        let id = k.i.unwrap_or(0);
        match k.a.unwrap_or(b't') {
            a @ (b't' | b'T') => {
                if k.t.is_some_and(|t| t != b'd') {
                    log::warn!("kitty graphics: transmission medium not supported");
                    return;
                }
                // Images without an id would need a reply to be usable.
                if id == 0 {
                    return;
                }
                if payload.len() > self.max_encoded {
                    log::warn!("kitty graphics: image {id} over the size limit dropped");
                    return;
                }
                let up = Upload {
                    id,
                    format: k.f,
                    width: k.s,
                    height: k.v,
                    zlib: k.o == Some(b'z'),
                    data: payload.to_vec(),
                    place: (a == b'T' && k.u == 1).then_some((k.p, k.c, k.r)),
                };
                if k.m == 1 {
                    self.upload = Some(up);
                } else {
                    on(Command::Transmit(up));
                }
            }
            b'p' if k.u == 1 && id != 0 => on(Command::Place {
                id,
                pid: k.p,
                cols: k.c,
                rows: k.r,
            }),
            b'd' => on(Command::Delete {
                what: k.d.unwrap_or(b'a'),
                id,
                pid: k.p,
            }),
            _ => {}
        }
    }
}

/// Standard base64 (padding and line breaks ignored).
fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in text {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn check_size(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_SOURCE_PIXELS {
        return Err(format!("image size {w}x{h} not accepted"));
    }
    Ok(())
}

/// Decodes an upload into an image no larger than `max_side` on either side.
fn decode(up: &Upload, max_side: usize) -> Result<ColorImage, String> {
    let raw = base64_decode(&up.data).ok_or("invalid base64")?;
    let raw = if up.zlib {
        let limit = (MAX_SOURCE_PIXELS * 4) as usize;
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&raw, limit)
            .map_err(|e| format!("zlib: {e:?}"))?
    } else {
        raw
    };
    let (w, h, rgba) = match up.format {
        100 => decode_png(raw)?,
        f @ (24 | 32) => {
            let (w, h) = (up.width, up.height);
            check_size(w, h)?;
            let bpp = if f == 32 { 4 } else { 3 };
            if raw.len() != w as usize * h as usize * bpp {
                return Err(format!("{} bytes for a {w}x{h} f={f} image", raw.len()));
            }
            let rgba = if f == 32 { raw } else { rgb_to_rgba(&raw) };
            (w as usize, h as usize, rgba)
        }
        f => return Err(format!("format f={f} not supported")),
    };
    let (w, h, rgba) = downscale(w, h, rgba, max_side);
    Ok(ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let (pixels, _) = rgb.as_chunks::<3>();
    pixels
        .iter()
        .flat_map(|&[r, g, b]| [r, g, b, 255])
        .collect()
}

fn decode_png(raw: Vec<u8>) -> Result<(usize, usize, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(Cursor::new(raw));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| format!("png: {e}"))?;
    let (w, h) = reader.info().size();
    check_size(w, h)?;
    let size = reader.output_buffer_size().ok_or("png: image too large")?;
    let mut buf = vec![0; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png: {e}"))?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => rgb_to_rgba(&buf),
        png::ColorType::GrayscaleAlpha => {
            let (pixels, _) = buf.as_chunks::<2>();
            pixels.iter().flat_map(|&[g, a]| [g, g, g, a]).collect()
        }
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("png: unexpanded palette".into()),
    };
    Ok((w as usize, h as usize, rgba))
}

/// Box-filters `rgba` down by the smallest integer factor that fits `max_side`.
fn downscale(w: usize, h: usize, rgba: Vec<u8>, max_side: usize) -> (usize, usize, Vec<u8>) {
    let k = w.max(h).div_ceil(max_side.max(1));
    if k <= 1 {
        return (w, h, rgba);
    }
    let (nw, nh) = ((w / k).max(1), (h / k).max(1));
    let mut out = Vec::with_capacity(nw * nh * 4);
    for y in 0..nh {
        for x in 0..nw {
            let mut sum = [0u32; 4];
            let mut n = 0;
            for sy in y * k..((y + 1) * k).min(h) {
                for sx in x * k..((x + 1) * k).min(w) {
                    let p = (sy * w + sx) * 4;
                    for (s, &v) in sum.iter_mut().zip(&rgba[p..p + 4]) {
                        *s += u32::from(v);
                    }
                    n += 1;
                }
            }
            out.extend(sum.map(|s| (s / n.max(1)) as u8));
        }
    }
    (nw, nh, out)
}

enum ImageState {
    /// Decoding on a worker thread.
    Pending,
    Ready {
        size: [usize; 2],
        /// Decoded pixels until the texture is created (UI thread, first paint).
        pixels: Option<ColorImage>,
        texture: Option<TextureHandle>,
    },
}

struct Image {
    /// Distinguishes re-uploads of the same id (a stale decode result is dropped).
    generation: u64,
    /// [`Store::tick`] of the last paint (LRU).
    used: u64,
    bytes: usize,
    state: ImageState,
}

#[derive(Clone, Copy)]
struct Placement {
    cols: u32,
    rows: u32,
    /// Creation order: the fallback for an unknown placement id is the newest.
    seq: u64,
}

/// Images and virtual placements of one terminal.
pub(crate) struct Store {
    images: HashMap<u32, Image>,
    placements: HashMap<(u32, u32), Placement>,
    /// Monotonic counter: generations, placement order, paint ticks.
    tick: u64,
    decoded_bytes: usize,
    max_bytes: usize,
}

impl Store {
    fn new() -> Store {
        Store {
            images: HashMap::new(),
            placements: HashMap::new(),
            tick: 0,
            decoded_bytes: 0,
            max_bytes: MAX_DECODED_TOTAL,
        }
    }

    fn next(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn remove_image(&mut self, id: u32) {
        if let Some(img) = self.images.remove(&id) {
            self.decoded_bytes -= img.bytes;
        }
    }

    /// Starts a (re-)upload of `id`; returns its generation.
    fn begin(&mut self, id: u32) -> u64 {
        self.remove_image(id);
        let generation = self.next();
        self.images.insert(
            id,
            Image {
                generation,
                used: generation,
                bytes: 0,
                state: ImageState::Pending,
            },
        );
        generation
    }

    /// Stores a decode result; false if the upload was deleted or replaced meanwhile.
    fn finish(&mut self, id: u32, generation: u64, result: Result<ColorImage, String>) -> bool {
        if self
            .images
            .get(&id)
            .is_none_or(|i| i.generation != generation)
        {
            return false;
        }
        let pixels = match result {
            Ok(p) => p,
            Err(e) => {
                log::warn!("kitty graphics: image {id}: {e}");
                self.remove_image(id);
                return false;
            }
        };
        let bytes = pixels.pixels.len() * 4;
        let used = self.next();
        if let Some(img) = self.images.get_mut(&id) {
            img.bytes = bytes;
            img.used = used;
            img.state = ImageState::Ready {
                size: pixels.size,
                pixels: Some(pixels),
                texture: None,
            };
        }
        self.decoded_bytes += bytes;
        self.evict(id);
        true
    }

    /// Drops least recently painted images (except `keep`) until the byte cap holds.
    fn evict(&mut self, keep: u32) {
        while self.decoded_bytes > self.max_bytes {
            let victim = self
                .images
                .iter()
                .filter(|(id, img)| **id != keep && img.bytes > 0)
                .min_by_key(|(_, img)| img.used)
                .map(|(id, _)| *id);
            let Some(victim) = victim else { break };
            log::debug!("kitty graphics: evicting image {victim}");
            self.remove_image(victim);
            self.placements.retain(|(id, _), _| *id != victim);
        }
    }

    fn place(&mut self, id: u32, pid: u32, cols: u32, rows: u32) {
        let seq = self.next();
        self.placements
            .insert((id, pid), Placement { cols, rows, seq });
    }

    /// `a=d`: lowercase deletes placements, uppercase also frees the image data.
    fn delete(&mut self, what: u8, id: u32, pid: u32) {
        match what {
            b'a' => self.placements.clear(),
            b'A' => {
                self.placements.clear();
                self.images.clear();
                self.decoded_bytes = 0;
            }
            b'i' | b'I' => {
                self.placements
                    .retain(|&(i, p), _| i != id || (pid != 0 && p != pid));
                if what == b'I' && !self.placements.keys().any(|&(i, _)| i == id) {
                    self.remove_image(id);
                }
            }
            // Deletes by cell/position/z-index/number target direct placements only.
            _ => {}
        }
    }

    /// Box (cols, rows) of placement (id, pid); unknown pid → the image's newest placement.
    fn placement(&self, id: u32, pid: u32) -> Option<Placement> {
        self.placements.get(&(id, pid)).copied().or_else(|| {
            self.placements
                .iter()
                .filter(|((i, _), _)| *i == id)
                .max_by_key(|(_, p)| p.seq)
                .map(|(_, p)| *p)
        })
    }

    /// Texture, pixel size and placement box (cells, 0 = unspecified) for placeholder cells
    /// of (id, pid); creates the texture on first use. UI thread only.
    pub(crate) fn target(
        &mut self,
        ctx: &egui::Context,
        id: u32,
        pid: u32,
    ) -> Option<(TextureId, [usize; 2], [u32; 2])> {
        let place = self.placement(id, pid)?;
        let tick = self.tick;
        let img = self.images.get_mut(&id)?;
        img.used = tick;
        let ImageState::Ready {
            size,
            pixels,
            texture,
        } = &mut img.state
        else {
            return None;
        };
        if texture.is_none() {
            let pixels = pixels.take()?;
            *texture =
                Some(ctx.load_texture(format!("kitty-image-{id}"), pixels, TextureOptions::LINEAR));
        }
        let texture = texture.as_ref()?;
        Some((texture.id(), *size, [place.cols, place.rows]))
    }
}

/// Per-terminal image store shared by the PTY thread, decode workers and the UI.
pub(crate) struct Graphics {
    store: Mutex<Store>,
    ctx: egui::Context,
    wake: crate::wake::Wake,
}

impl Graphics {
    pub(crate) fn new(ctx: egui::Context, wake: crate::wake::Wake) -> Graphics {
        Graphics {
            store: Mutex::new(Store::new()),
            ctx,
            wake,
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Store for one paint pass (advances the LRU clock).
    pub(crate) fn paint_lock(&self) -> MutexGuard<'_, Store> {
        let mut store = self.store();
        store.next();
        store
    }

    /// Applies a command from the PTY thread; uploads decode on a worker thread.
    pub(crate) fn apply(self: &Arc<Self>, cmd: Command) {
        match cmd {
            Command::Transmit(up) => {
                let (id, generation) = {
                    let mut store = self.store();
                    let generation = store.begin(up.id);
                    if let Some((pid, cols, rows)) = up.place {
                        store.place(up.id, pid, cols, rows);
                    }
                    (up.id, generation)
                };
                let graphics = self.clone();
                let spawned = std::thread::Builder::new()
                    .name("deck-image-decode".into())
                    .spawn(move || {
                        let max_side = graphics.ctx.input(|i| i.max_texture_side).min(MAX_SIDE);
                        let result = decode(&up, max_side);
                        if graphics.store().finish(id, generation, result) {
                            graphics.wake.now();
                        }
                    });
                if let Err(e) = spawned {
                    log::warn!("kitty graphics: decode thread: {e}");
                    self.store().remove_image(id);
                }
            }
            Command::Place {
                id,
                pid,
                cols,
                rows,
            } => {
                self.store().place(id, pid, cols, rows);
                self.wake.now();
            }
            Command::Delete { what, id, pid } => {
                self.store().delete(what, id, pid);
                self.wake.now();
            }
        }
    }
}

/// One placeholder cell of the visible grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placeholder {
    /// Viewport cell.
    pub row: usize,
    pub col: usize,
    pub id: u32,
    pub pid: u32,
    /// Cell of the image box this cell shows.
    pub img_row: u32,
    pub img_col: u32,
}

/// 24-bit (or 8-bit indexed) id carried by a color.
fn color_id(c: Color) -> Option<u32> {
    match c {
        Color::Spec(rgb) => {
            Some((u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b))
        }
        Color::Indexed(n) => Some(u32::from(n)),
        Color::Named(_) => None,
    }
}

/// Decodes a placeholder cell from its raw colors and combining chars. `left` is the
/// placeholder directly left of it (same row), used to infer missing diacritics.
pub(crate) fn placeholder(
    row: usize,
    col: usize,
    fg: Color,
    underline: Option<Color>,
    zerowidth: &[char],
    left: Option<&Placeholder>,
) -> Option<Placeholder> {
    let id24 = color_id(fg)?;
    let pid = underline.and_then(color_id).unwrap_or(0);
    let left = left
        .filter(|l| l.row == row && l.col + 1 == col && l.id & 0xFF_FFFF == id24 && l.pid == pid);
    let mut marks = zerowidth.iter().map_while(|&c| diacritic(c));
    let (r, c, msb) = (marks.next(), marks.next(), marks.next());
    let (img_row, img_col, msb) = match (r, c, msb) {
        (None, _, _) => left.map_or((0, 0, 0), |l| (l.img_row, l.img_col + 1, l.id >> 24)),
        (Some(r), None, _) => left
            .filter(|l| l.img_row == r)
            .map_or((r, 0, 0), |l| (r, l.img_col + 1, l.id >> 24)),
        (Some(r), Some(c), None) => left
            .filter(|l| l.img_row == r && l.img_col + 1 == c)
            .map_or((r, c, 0), |l| (r, c, l.id >> 24)),
        (Some(r), Some(c), Some(m)) => (r, c, m),
    };
    let id = id24 | ((msb & 0xFF) << 24);
    (id != 0).then_some(Placeholder {
        row,
        col,
        id,
        pid,
        img_row,
        img_col,
    })
}

/// Placement box in cells; missing sides follow from the image size at the cell size.
pub(crate) fn resolve_box(box_cells: [u32; 2], img: [usize; 2], cell: [f32; 2]) -> [u32; 2] {
    let [w, h] = [img[0] as f32, img[1] as f32];
    match box_cells {
        [0, 0] => [(w / cell[0]).ceil() as u32, (h / cell[1]).ceil() as u32],
        [cols, 0] => [
            cols,
            (cols as f32 * cell[0] * h / w / cell[1]).ceil() as u32,
        ],
        [0, rows] => [
            (rows as f32 * cell[1] * w / h / cell[0]).ceil() as u32,
            rows,
        ],
        b => b,
    }
}

/// Part of the image shown by cell (`row`, `col`) of a `box_cells` placement: the image is
/// scaled to fit the box keeping its aspect ratio, centered. Returns the drawn rect relative
/// to the cell's top-left (pixels) and its UV rect; None if the cell shows no image.
pub(crate) fn cell_quad(
    box_cells: [u32; 2],
    img: [usize; 2],
    cell: [f32; 2],
    row: u32,
    col: u32,
) -> Option<(Rect, Rect)> {
    if row >= box_cells[1] || col >= box_cells[0] || img[0] == 0 || img[1] == 0 {
        return None;
    }
    let (bw, bh) = (box_cells[0] as f32 * cell[0], box_cells[1] as f32 * cell[1]);
    let (iw, ih) = (img[0] as f32, img[1] as f32);
    let scale = (bw / iw).min(bh / ih);
    let (fw, fh) = ((iw * scale).round().max(1.0), (ih * scale).round().max(1.0));
    let fit = Rect::from_min_size(
        pos2(((bw - fw) / 2.0).round(), ((bh - fh) / 2.0).round()),
        vec2(fw, fh),
    );
    let cell_min = pos2(col as f32 * cell[0], row as f32 * cell[1]);
    let vis = Rect::from_min_size(cell_min, vec2(cell[0], cell[1])).intersect(fit);
    if vis.width() <= 0.0 || vis.height() <= 0.0 {
        return None;
    }
    let uv = |p: Pos2| pos2((p.x - fit.min.x) / fw, (p.y - fit.min.y) / fh);
    Some((
        vis.translate(-cell_min.to_vec2()),
        Rect::from_min_max(uv(vis.min), uv(vis.max)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::{NamedColor, Rgb};

    fn scan(chunks: &[&[u8]]) -> Vec<Command> {
        let mut s = ApcScanner::default();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, |cmd| out.push(cmd));
        }
        out
    }

    fn upload(id: u32, data: &[u8]) -> Upload {
        Upload {
            id,
            format: 100,
            data: data.to_vec(),
            ..Upload::default()
        }
    }

    #[test]
    fn single_and_chunked_transmission() {
        assert_eq!(
            scan(&[b"x\x1b_Ga=t,f=100,q=2,i=7;QUJD\x1b\\y"]),
            [Command::Transmit(upload(7, b"QUJD"))]
        );
        let stream: &[u8] = b"\x1b_Ga=t,f=100,q=2,i=7,m=1;AAAA\x1b\\\x1b[0m\
            \x1b_Gq=2,m=1;BBBB\x1b\\\x1b_Gq=2,m=0;CC==\x1b\\";
        let want = [Command::Transmit(upload(7, b"AAAABBBBCC=="))];
        assert_eq!(scan(&[stream]), want);
        // Split at every possible position, and byte by byte.
        for at in 0..stream.len() {
            assert_eq!(scan(&[&stream[..at], &stream[at..]]), want, "split at {at}");
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(scan(&bytes), want);
    }

    #[test]
    fn placement_and_delete_commands() {
        assert_eq!(
            scan(&[b"\x1b_Ga=p,U=1,q=2,i=5,p=5,c=10,r=4\x1b\\"]),
            [Command::Place {
                id: 5,
                pid: 5,
                cols: 10,
                rows: 4
            }]
        );
        // Direct placements are not supported.
        assert_eq!(scan(&[b"\x1b_Ga=p,i=5,c=10,r=4\x1b\\"]), []);
        assert_eq!(
            scan(&[b"\x1b_Ga=d,d=I,i=5,q=2\x1b\\\x1b_Ga=d,q=2\x1b\\"]),
            [
                Command::Delete {
                    what: b'I',
                    id: 5,
                    pid: 0
                },
                Command::Delete {
                    what: b'a',
                    id: 0,
                    pid: 0
                }
            ]
        );
        let mut t = upload(3, b"");
        t.format = 32;
        t.width = 1;
        t.height = 1;
        t.place = Some((9, 2, 1));
        assert_eq!(
            scan(&[b"\x1b_Ga=T,U=1,i=3,p=9,c=2,r=1,s=1,v=1\x1b\\"]),
            [Command::Transmit(t)]
        );
    }

    #[test]
    fn refused_and_malformed() {
        // File / shm mediums, no id, other APC strings, unterminated strings.
        assert_eq!(
            scan(&[
                b"\x1b_Ga=t,t=f,i=1;L3RtcA==\x1b\\\x1b_Ga=t,t=s,i=1;eA==\x1b\\",
                b"\x1b_Ga=t,f=100;QUJD\x1b\\\x1b_Xi=1\x1b\\\x1b_Ga=t,i=1;QU\x1b[0m"
            ]),
            []
        );
        // An unterminated chunked upload is dropped; the following command still works.
        assert_eq!(
            scan(&[b"\x1b_Ga=t,i=1,m=1;AAAA\x1b\\\x1b_Gm=0;BB\x1bc\x1b_Ga=t,i=2;CC\x1b\\"]),
            [Command::Transmit({
                let mut u = upload(2, b"CC");
                u.format = 32;
                u
            })]
        );
    }

    #[test]
    fn size_limit() {
        let mut s = ApcScanner {
            max_encoded: 8,
            ..ApcScanner::default()
        };
        let mut out = Vec::new();
        s.feed(b"\x1b_Ga=t,i=1;AAAAAAAAAAAA\x1b\\", |c| out.push(c));
        s.feed(
            b"\x1b_Ga=t,i=2,m=1;AAAA\x1b\\\x1b_Gm=1;AAAA\x1b\\\x1b_Gm=0;AAAA\x1b\\",
            |c| out.push(c),
        );
        assert_eq!(out, []);
        s.feed(b"\x1b_Ga=t,i=3;AAAA\x1b\\", |c| out.push(c));
        assert_eq!(out.len(), 1);
        // Oversized dimensions are rejected before decoding.
        let big = Upload {
            id: 1,
            format: 32,
            width: 1 << 13,
            height: 1 << 13,
            data: b"AAAA".to_vec(),
            ..Upload::default()
        };
        assert!(decode(&big, MAX_SIDE).is_err());
    }

    #[test]
    fn decodes_raw_and_png() {
        // 2x1 RGB: red, green.
        let rgb = Upload {
            id: 1,
            format: 24,
            width: 2,
            height: 1,
            data: b"/wAAAP8A".to_vec(),
            ..Upload::default()
        };
        let img = decode(&rgb, MAX_SIDE).unwrap();
        assert_eq!(img.size, [2, 1]);
        assert_eq!(img.pixels[0], egui::Color32::RED);
        // Wrong length.
        let short = Upload {
            data: b"/wAA".to_vec(),
            ..rgb.clone()
        };
        assert!(decode(&short, MAX_SIDE).is_err());
        // 4x2 RGBA PNG, downscaled by 2.
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 4, 2);
            enc.set_color(png::ColorType::Rgba);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255; 32]).unwrap();
        }
        let png = Upload {
            id: 2,
            format: 100,
            data: base64(&png_bytes),
            ..Upload::default()
        };
        assert_eq!(decode(&png, MAX_SIDE).unwrap().size, [4, 2]);
        assert_eq!(decode(&png, 2).unwrap().size, [2, 1]);
    }

    fn base64(bytes: &[u8]) -> Vec<u8> {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        for c in bytes.chunks(3) {
            let n =
                (c.iter().enumerate()).fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
            for i in 0..4 {
                out.push(if i <= c.len() {
                    A[((n >> (18 - 6 * i)) & 63) as usize]
                } else {
                    b'='
                });
            }
        }
        out
    }

    #[test]
    fn base64_roundtrip() {
        for len in 0..10 {
            let data: Vec<u8> = (0..len).map(|i: u8| i.wrapping_mul(37)).collect();
            assert_eq!(base64_decode(&base64(&data)).unwrap(), data);
        }
        assert_eq!(base64_decode(b"QU*D"), None);
    }

    fn ready(store: &mut Store, id: u32, w: usize) {
        let generation = store.begin(id);
        assert!(store.finish(
            id,
            generation,
            Ok(ColorImage::filled([w, 1], egui::Color32::RED))
        ));
    }

    #[test]
    fn delete_semantics() {
        let mut s = Store::new();
        ready(&mut s, 1, 4);
        ready(&mut s, 2, 4);
        s.place(1, 1, 4, 2);
        s.place(1, 7, 2, 1);
        s.place(2, 2, 3, 3);
        // Unknown pid (or 0) falls back to the image's newest placement.
        assert_eq!(s.placement(1, 1).map(|p| p.cols), Some(4));
        assert_eq!(s.placement(1, 0).map(|p| p.cols), Some(2));
        s.delete(b'i', 1, 7);
        assert_eq!(s.placement(1, 0).map(|p| p.cols), Some(4));
        // d=I frees the image once no placement is left.
        s.delete(b'I', 1, 0);
        assert!(s.placement(1, 1).is_none());
        assert!(!s.images.contains_key(&1));
        assert_eq!(s.decoded_bytes, 16);
        // d=a keeps image data, d=A frees everything.
        s.delete(b'a', 0, 0);
        assert!(s.placements.is_empty() && s.images.contains_key(&2));
        s.delete(b'A', 0, 0);
        assert!(s.images.is_empty());
        assert_eq!(s.decoded_bytes, 0);
        // A decode finishing after its image was deleted or replaced is dropped.
        let old = s.begin(3);
        let new = s.begin(3);
        assert!(!s.finish(3, old, Ok(ColorImage::filled([1, 1], egui::Color32::RED))));
        s.delete(b'I', 3, 0);
        assert!(!s.finish(3, new, Ok(ColorImage::filled([1, 1], egui::Color32::RED))));
        assert_eq!(s.decoded_bytes, 0);
    }

    #[test]
    fn lru_eviction() {
        let mut s = Store::new();
        s.max_bytes = 40;
        ready(&mut s, 1, 4);
        ready(&mut s, 2, 4);
        s.place(1, 1, 1, 1);
        // Image 1 painted more recently than 2.
        s.tick += 1;
        s.images.get_mut(&1).unwrap().used = s.tick;
        ready(&mut s, 3, 4);
        assert!(s.images.contains_key(&1) && s.images.contains_key(&3));
        assert!(!s.images.contains_key(&2));
        assert_eq!(s.decoded_bytes, 32);
        // An image over the whole cap evicts everything else but is kept itself.
        ready(&mut s, 4, 20);
        assert_eq!(s.images.keys().copied().collect::<Vec<_>>(), [4]);
        assert!(s.placements.is_empty());
    }

    fn d(i: usize) -> char {
        char::from_u32(DIACRITICS[i]).unwrap()
    }

    #[test]
    fn placeholder_ids() {
        let fg = Color::Spec(Rgb { r: 1, g: 2, b: 3 });
        let ul = Some(Color::Spec(Rgb { r: 0, g: 0, b: 9 }));
        let p = placeholder(0, 0, fg, ul, &[d(2), d(5)], None).unwrap();
        assert_eq!((p.id, p.pid, p.img_row, p.img_col), (0x010203, 9, 2, 5));
        let p = placeholder(0, 0, Color::Indexed(42), None, &[d(0), d(0)], None).unwrap();
        assert_eq!((p.id, p.pid), (42, 0));
        // Third diacritic: most significant byte of the id.
        let p = placeholder(0, 0, fg, None, &[d(0), d(0), d(7)], None).unwrap();
        assert_eq!(p.id, 0x0701_0203);
        // Default foreground / id 0: not an image cell.
        assert!(placeholder(0, 0, Color::Named(NamedColor::Foreground), None, &[], None).is_none());
        assert!(placeholder(0, 0, Color::Indexed(0), None, &[], None).is_none());
        assert_eq!(diacritic('\u{305}'), Some(0));
        assert_eq!(diacritic('\u{30D}'), Some(1));
        assert_eq!(diacritic('a'), None);
    }

    #[test]
    fn placeholder_inference() {
        let fg = Color::Spec(Rgb { r: 0, g: 0, b: 5 });
        let first = placeholder(3, 4, fg, None, &[d(1), d(2), d(9)], None).unwrap();
        // No diacritics: same row, next column, same msb.
        let p = placeholder(3, 5, fg, None, &[], Some(&first)).unwrap();
        assert_eq!((p.img_row, p.img_col, p.id), (1, 3, 0x0900_0005));
        // Row only: column continues when the row matches.
        let p = placeholder(3, 5, fg, None, &[d(1)], Some(&first)).unwrap();
        assert_eq!((p.img_row, p.img_col), (1, 3));
        let p = placeholder(3, 5, fg, None, &[d(2)], Some(&first)).unwrap();
        assert_eq!((p.img_row, p.img_col, p.id), (2, 0, 5));
        // Row + column continuing the left cell inherit its msb.
        let p = placeholder(3, 5, fg, None, &[d(1), d(3)], Some(&first)).unwrap();
        assert_eq!(p.id, 0x0900_0005);
        // Left neighbour of another image/placement, or not adjacent: no inference.
        let other = Color::Spec(Rgb { r: 0, g: 0, b: 6 });
        let p = placeholder(3, 5, other, None, &[], Some(&first)).unwrap();
        assert_eq!((p.img_row, p.img_col), (0, 0));
        let p = placeholder(3, 6, fg, None, &[], Some(&first)).unwrap();
        assert_eq!((p.img_row, p.img_col), (0, 0));
    }

    #[test]
    fn aspect_fit_cell_quads() {
        let cell = [10.0, 20.0];
        // 4x2-cell box = 40x40 px; a 200x100 image fits as 40x20 at y 10..30.
        let (r, uv) = cell_quad([4, 2], [200, 100], cell, 0, 1).unwrap();
        assert_eq!(r, Rect::from_min_max(pos2(0.0, 10.0), pos2(10.0, 20.0)));
        assert_eq!(uv, Rect::from_min_max(pos2(0.25, 0.0), pos2(0.5, 0.5)));
        let (r, uv) = cell_quad([4, 2], [200, 100], cell, 1, 3).unwrap();
        assert_eq!(r, Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 10.0)));
        assert_eq!(uv, Rect::from_min_max(pos2(0.75, 0.5), pos2(1.0, 1.0)));
        // Tall image: centered horizontally, side cells empty.
        assert!(cell_quad([4, 2], [100, 400], cell, 0, 0).is_none());
        let (r, uv) = cell_quad([4, 2], [100, 400], cell, 0, 1).unwrap();
        assert_eq!(r, Rect::from_min_max(pos2(5.0, 0.0), pos2(10.0, 20.0)));
        assert_eq!(uv, Rect::from_min_max(pos2(0.0, 0.0), pos2(0.5, 0.5)));
        // Outside the box.
        assert!(cell_quad([4, 2], [200, 100], cell, 2, 0).is_none());
        assert!(cell_quad([4, 2], [200, 100], cell, 0, 4).is_none());
        // Missing box sides.
        assert_eq!(resolve_box([0, 0], [95, 41], cell), [10, 3]);
        assert_eq!(resolve_box([4, 0], [200, 100], cell), [4, 1]);
        assert_eq!(resolve_box([0, 2], [200, 100], cell), [8, 2]);
    }

    #[test]
    fn asynchronous_decode_wakes_a_headless_reader_when_pixels_are_ready() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (tx, rx) = mpsc::channel();
        let graphics = Arc::new(Graphics::new(
            egui::Context::default(),
            crate::wake::Wake::Headless(Arc::new(move |delay| {
                let _ = tx.send(delay);
            })),
        ));
        graphics.apply(Command::Transmit(Upload {
            id: 11,
            format: 24,
            width: 2,
            height: 1,
            data: b"/wAAAP8A".to_vec(),
            ..Upload::default()
        }));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5))
                .expect("decode wake"),
            Duration::ZERO
        );
        let store = graphics.store();
        let ImageState::Ready {
            pixels: Some(pixels),
            ..
        } = &store.images[&11].state
        else {
            panic!("the decode wake must publish ready pixels");
        };
        assert_eq!(pixels.size, [2, 1]);
        assert_eq!(pixels.pixels, [egui::Color32::RED, egui::Color32::GREEN]);
    }
}

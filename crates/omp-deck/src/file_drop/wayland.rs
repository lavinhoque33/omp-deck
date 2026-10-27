//! Wayland drop target: `wl_data_device` on winit's own `wl_display`. Accepts `text/uri-list`
//! offers over the window's surface, reports the drag position, and on drop reads the list and
//! reports the local file paths in it.

use std::ffi::{OsStr, c_void};
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};

pub const URI_LIST: &str = "text/uri-list";

/// A uri-list larger than this is not a list of dropped files.
const MAX_LIST: u64 = 4 << 20;

/// Drag state for the UI thread; positions are surface-local logical pixels.
pub enum Event {
    Hover { x: f64, y: f64 },
    Left,
    Dropped { x: f64, y: f64, paths: Vec<PathBuf> },
}

/// Starts the drop-target thread for winit's `display` and the window's `surface` (raw pointers
/// from `notify::wayland::window_pointers`; the window and its connection live until the process
/// exits). Failures only lose file drops, so they are logged.
pub fn start(display: usize, surface: usize, tx: Sender<Event>, ctx: egui::Context) {
    let spawned = std::thread::Builder::new()
        .name("omp-deck-dnd".into())
        .spawn(move || {
            if let Err(e) = run(display, surface, tx, ctx) {
                log::info!("Wayland file drops unavailable: {e}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("starting the file drop thread failed: {e}");
    }
}

fn run(
    display: usize,
    surface: usize,
    tx: Sender<Event>,
    ctx: egui::Context,
) -> Result<(), String> {
    // SAFETY: valid `wl_display*` of winit's connection (see `start`); the foreign backend
    // never closes it.
    let backend = unsafe { Backend::from_foreign_display(display as *mut c_void as *mut _) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let manager: WlDataDeviceManager = globals
        .bind(&qh, 1..=3, ())
        .map_err(|e| format!("no wl_data_device_manager: {e}"))?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("no wl_seat: {e}"))?;
    let _device = manager.get_data_device(&seat, &qh, ());
    // SAFETY: `surface` is winit's live `wl_surface*` (see `start`).
    let id =
        unsafe { ObjectId::from_ptr(WlSurface::interface(), surface as *mut c_void as *mut _) }
            .map_err(|e| format!("surface: {e}"))?;
    let surface = WlSurface::from_id(&conn, id).map_err(|e| format!("surface: {e}"))?;
    let mut state = State {
        conn: conn.clone(),
        surface,
        v3: manager.version() >= 3,
        drag: None,
        pos: (0.0, 0.0),
        tx,
        ctx,
    };
    loop {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| format!("dispatch: {e}"))?;
    }
}

struct State {
    conn: Connection,
    surface: WlSurface,
    /// `wl_data_offer.set_actions` / `finish` exist (data device manager v3).
    v3: bool,
    /// The offer of the drag over the window, and whether it is accepted.
    drag: Option<(WlDataOffer, bool)>,
    /// Last drag position.
    pos: (f64, f64),
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl State {
    fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            self.ctx.request_repaint();
        }
    }

    fn end_drag(&mut self) {
        if let Some((offer, accepted)) = self.drag.take() {
            offer.destroy();
            if accepted {
                self.send(Event::Left);
            }
        }
    }

    /// Reads the dropped list off the dispatch thread (the source may be slow or stall), then
    /// finishes the offer and reports the paths.
    fn receive(&self, offer: WlDataOffer) {
        let (mut reader, writer) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("file drop: pipe: {e}");
                offer.destroy();
                return;
            }
        };
        offer.receive(URI_LIST.to_owned(), writer.as_fd());
        drop(writer);
        let _ = self.conn.flush();
        let (conn, v3, tx, ctx) = (
            self.conn.clone(),
            self.v3,
            self.tx.clone(),
            self.ctx.clone(),
        );
        let (x, y) = self.pos;
        let spawned = std::thread::Builder::new()
            .name("omp-deck-dnd-read".into())
            .spawn(move || {
                let mut list = Vec::new();
                let read = (&mut reader).take(MAX_LIST).read_to_end(&mut list);
                if v3 {
                    offer.finish();
                }
                offer.destroy();
                let _ = conn.flush();
                match read {
                    Ok(_) => {
                        let paths = file_paths(&list);
                        if !paths.is_empty() && tx.send(Event::Dropped { x, y, paths }).is_ok() {
                            ctx.request_repaint();
                        }
                    }
                    Err(e) => log::warn!("file drop: reading the uri-list: {e}"),
                }
            });
        if let Err(e) = spawned {
            log::warn!("file drop: reader thread: {e}");
        }
    }
}

/// Mime types an offer announced (before its `enter`).
#[derive(Default)]
struct OfferData {
    uri_list: AtomicBool,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlDataDeviceManager,
        _: <WlDataDeviceManager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataOffer, OfferData> for State {
    fn event(
        _: &mut Self,
        _: &WlDataOffer,
        event: wl_data_offer::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && mime_type == URI_LIST
        {
            data.uri_list.store(true, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, OfferData::default())
    ]);

    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                state.end_drag();
                let Some(offer) = id else { return };
                let files = offer
                    .data::<OfferData>()
                    .is_some_and(|d| d.uri_list.load(Ordering::Relaxed));
                let accepted = files && surface == state.surface;
                if accepted {
                    offer.accept(serial, Some(URI_LIST.to_owned()));
                    if state.v3 {
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                    state.pos = (x, y);
                    state.send(Event::Hover { x, y });
                } else {
                    offer.accept(serial, None);
                }
                state.drag = Some((offer, accepted));
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if matches!(state.drag, Some((_, true))) {
                    state.pos = (x, y);
                    state.send(Event::Hover { x, y });
                }
            }
            wl_data_device::Event::Leave => state.end_drag(),
            wl_data_device::Event::Drop => match state.drag.take() {
                Some((offer, true)) => {
                    state.send(Event::Left);
                    state.receive(offer);
                }
                Some((offer, false)) => offer.destroy(),
                None => {}
            },
            // Clipboard offers: the clipboard is read elsewhere.
            wl_data_device::Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }
}

/// Local file paths in a `text/uri-list` (RFC 2483: CRLF lines, `#` comments); other URIs and
/// files on other hosts are skipped.
fn file_paths(list: &[u8]) -> Vec<PathBuf> {
    list.split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(|line| {
            let rest = strip_prefix_ignore_case(line, b"file://")?;
            let slash = rest.iter().position(|&b| b == b'/')?;
            let host = &rest[..slash];
            if !(host.is_empty() || host.eq_ignore_ascii_case(b"localhost")) {
                return None;
            }
            let path = percent_decode(&rest[slash..]);
            Some(PathBuf::from(OsStr::from_bytes(&path)))
        })
        .collect()
}

fn strip_prefix_ignore_case<'a>(s: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    (s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &s[prefix.len()..])
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let (Some(h), Some(l)) = (
                s.get(i + 1).copied().and_then(hex),
                s.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_yields_local_paths_only() {
        let list = b"# from Dolphin\r\nfile:///home/me/My%20Pics/shot%2B1.png\r\n\
            FILE://localhost/tmp/a%zz%4\r\nfile://otherhost/etc/x\r\nhttps://example.com/a.png\r\n\
            file:///home/me/%E6%BC%A2.txt\n\r\n";
        assert_eq!(
            file_paths(list),
            [
                PathBuf::from("/home/me/My Pics/shot+1.png"),
                PathBuf::from("/tmp/a%zz%4"),
                PathBuf::from("/home/me/漢.txt"),
            ]
        );
    }
}

//! Raising the window on Wayland: winit cannot apply an external activation token, so the notifier
//! thread binds `xdg_activation_v1` on winit's own `wl_display` (foreign backend, separate event
//! queue) and activates winit's `wl_surface` with the token the notification server handed us.

use std::ffi::c_void;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

/// winit's display and surface pointers (as integers, so they can cross to the notifier thread);
/// `None` on X11 or when the handles are unavailable.
pub fn window_pointers(frame: &eframe::Frame) -> Option<(usize, usize)> {
    let window = frame.window_handle().ok()?.as_raw();
    let display = frame.display_handle().ok()?.as_raw();
    match (window, display) {
        (RawWindowHandle::Wayland(w), RawDisplayHandle::Wayland(d)) => {
            Some((d.display.as_ptr() as usize, w.surface.as_ptr() as usize))
        }
        _ => None,
    }
}

struct Queue;

impl Dispatch<WlRegistry, GlobalListContents> for Queue {
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

impl Dispatch<XdgActivationV1, ()> for Queue {
    fn event(
        _: &mut Self,
        _: &XdgActivationV1,
        _: <XdgActivationV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct Raiser {
    conn: Connection,
    activation: XdgActivationV1,
    surface: WlSurface,
    /// Keeps the queue the activation object was bound on alive.
    _queue: EventQueue<Queue>,
}

impl Raiser {
    /// Binds `xdg_activation_v1` on the window's display.
    ///
    /// `display` and `surface` come from [`window_pointers`] for the app's only window; that window
    /// (and its display connection) outlives the notifier thread's use of them, which stops when
    /// the process exits.
    pub fn new(display: usize, surface: usize) -> Result<Raiser, String> {
        // SAFETY: valid `wl_display*` of winit's connection (see the doc above); the foreign
        // backend never closes it.
        let backend = unsafe { Backend::from_foreign_display(display as *mut c_void as *mut _) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) =
            registry_queue_init::<Queue>(&conn).map_err(|e| format!("registry: {e}"))?;
        let qh = queue.handle();
        let activation: XdgActivationV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("the compositor has no xdg_activation_v1: {e}"))?;
        // SAFETY: `surface` is winit's live `wl_surface*` (see the doc above).
        let id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), surface as *mut c_void as *mut _) }
                .map_err(|e| format!("surface: {e}"))?;
        let surface = WlSurface::from_id(&conn, id).map_err(|e| format!("surface: {e}"))?;
        Ok(Raiser {
            conn,
            activation,
            surface,
            _queue: queue,
        })
    }

    /// Asks the compositor to activate the window with `token`.
    pub fn activate(&self, token: &str) -> Result<(), String> {
        self.activation.activate(token.to_owned(), &self.surface);
        self.conn.flush().map_err(|e| format!("flush: {e}"))
    }
}

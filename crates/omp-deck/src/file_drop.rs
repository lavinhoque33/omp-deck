//! File drag-and-drop from other applications. X11 and Windows: winit hands drops to egui
//! (`RawInput::dropped_files` / `hovered_files`). Wayland: winit 0.30 has no drag-and-drop, so a
//! thread binds `wl_data_device` on winit's own display (foreign backend, separate event queue,
//! like the window raiser) and [`FileDrops::feed`] puts its drops into egui's raw input, with the
//! drag position as the pointer position (the compositor sends no pointer events during a drag).
//! `deck_term::TerminalView` pastes the dropped paths into the terminal under the pointer.

#[cfg(target_os = "linux")]
mod wayland;

#[cfg(target_os = "linux")]
use std::sync::mpsc::{self, Receiver, Sender};

pub struct FileDrops {
    #[cfg(target_os = "linux")]
    attached: bool,
    #[cfg(target_os = "linux")]
    tx: Sender<wayland::Event>,
    #[cfg(target_os = "linux")]
    rx: Receiver<wayland::Event>,
    /// A Wayland drag is over the window.
    #[cfg(target_os = "linux")]
    hovering: bool,
}

impl FileDrops {
    pub fn new() -> FileDrops {
        #[cfg(target_os = "linux")]
        {
            let (tx, rx) = mpsc::channel();
            FileDrops {
                attached: false,
                tx,
                rx,
                hovering: false,
            }
        }
        #[cfg(not(target_os = "linux"))]
        FileDrops {}
    }

    /// Once the window exists: on Wayland, start receiving drops on its surface.
    pub fn attach_window(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        #[cfg(target_os = "linux")]
        {
            if self.attached {
                return;
            }
            self.attached = true;
            if let Some((display, surface)) = crate::notify::wayland::window_pointers(frame) {
                wayland::start(display, surface, self.tx.clone(), ctx.clone());
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (ctx, frame);
    }

    /// Adds what the Wayland thread received since the last frame to this frame's raw input.
    pub fn feed(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        #[cfg(target_os = "linux")]
        {
            use std::sync::Arc;

            // Surface coordinates are logical pixels; egui points are those over the zoom.
            let zoom = ctx.zoom_factor();
            let at = |x: f64, y: f64| egui::pos2(x as f32 / zoom, y as f32 / zoom);
            for event in self.rx.try_iter() {
                match event {
                    wayland::Event::Hover { x, y } => {
                        self.hovering = true;
                        raw.events.push(egui::Event::PointerMoved(at(x, y)));
                    }
                    wayland::Event::Left => {
                        if std::mem::take(&mut self.hovering) {
                            raw.events.push(egui::Event::PointerGone);
                        }
                    }
                    wayland::Event::Dropped { x, y, paths } => {
                        raw.events.push(egui::Event::PointerMoved(at(x, y)));
                        raw.dropped_files.extend(
                            paths
                                .into_iter()
                                .map(|p| Arc::new(DroppedPath(p)) as egui::DroppedFileHandle),
                        );
                    }
                }
            }
            if self.hovering {
                raw.hovered_files.push(egui::HoveredFile {
                    path: None,
                    mime: wayland::URI_LIST.to_owned(),
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (ctx, raw);
    }
}

/// A file dropped through the Wayland bridge.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct DroppedPath(std::path::PathBuf);

#[cfg(target_os = "linux")]
impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

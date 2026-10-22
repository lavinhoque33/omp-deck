//! The popup's Output tab and the markdown pane shared with the Task tab.
//!
//! The output file is read on worker threads only: a bounded preview (cap + 1 bytes, so
//! truncation is detectable), a cheap re-check that stats the file and re-reads only when its
//! `(mtime, len)` changed, and an unbounded read for Copy when the preview was cut.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use crate::icons::{ICON_FILE_TEXT, ICON_INFO};
use deck_core::agents::{AgentStatus, AgentSummary};
use egui::{Frame, Id, Margin, RichText, Ui};
use egui_commonmark::{CommonMarkCache, CommonMarkScrollOptions, CommonMarkViewer};

use crate::theme::Palette;
use crate::widgets::{Spin, empty_state, icon_text};

/// Output files larger than this are previewed, not rendered whole.
pub const PREVIEW_CAP: u64 = 4 * 1024 * 1024;
/// While the Output tab is open the file is re-checked this often (off the UI thread).
const RECHECK: Duration = Duration::from_secs(3);

/// `(mtime, len)` of the file a text was read from.
type Version = (Option<SystemTime>, u64);

/// Markdown viewer whose viewport cache is dropped whenever the width or the content changes
/// (egui_commonmark keeps page size and split points across frames).
#[derive(Default)]
pub struct MdPane {
    cache: CommonMarkCache,
    sig: Option<(i32, usize)>,
}

impl MdPane {
    /// `generation` identifies the content: any change of it invalidates the cached layout.
    pub fn show(&mut self, ui: &mut Ui, id: Id, text: &str, generation: usize) {
        let sig = (ui.available_width().round() as i32, generation);
        if self.sig != Some(sig) {
            self.cache.clear_viewer(&id);
            self.sig = Some(sig);
        }
        CommonMarkViewer::new().show_scrollable(
            id,
            ui,
            &mut self.cache,
            &CommonMarkScrollOptions::default().viewport_cache(true),
            text,
        );
    }
}

struct Loaded {
    text: String,
    /// File size when read (the preview may be shorter).
    total: u64,
    truncated: bool,
    version: Version,
}

type LoadResult = Result<Option<Loaded>, String>;

/// Reads `path`. `known`: the version already shown; an unchanged file answers `Ok(None)`
/// after a stat, without reading.
fn load(path: &Path, cap: Option<u64>, known: Option<Version>) -> LoadResult {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let version = (meta.modified().ok(), meta.len());
    if known == Some(version) {
        return Ok(None);
    }
    let mut buf = Vec::new();
    match cap {
        Some(c) => file.take(c + 1).read_to_end(&mut buf),
        None => (&file).read_to_end(&mut buf),
    }
    .map_err(|e| e.to_string())?;
    let truncated = cap.is_some_and(|c| buf.len() as u64 > c);
    if let (true, Some(c)) = (truncated, cap) {
        buf.truncate(c as usize);
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(Some(Loaded {
        text: if truncated {
            text
        } else {
            unwrap_json_string(text)
        },
        total: version.1.max(buf.len() as u64),
        truncated,
        version,
    }))
}

/// Some agents write their yield payload as one JSON string literal (`"line\n\nline"`); show it
/// as the text it encodes. Anything else (plain markdown, a truncated read) is shown verbatim.
pub fn unwrap_json_string(text: String) -> String {
    let t = text.trim();
    if t.len() >= 2
        && t.starts_with('"')
        && t.ends_with('"')
        && let Ok(s) = serde_json::from_str::<String>(t)
    {
        return s;
    }
    text
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

pub struct OutputPane {
    loaded: Option<Loaded>,
    error: Option<String>,
    loading: Option<Receiver<LoadResult>>,
    /// `(last_activity, status)` the current text was requested for.
    trigger: Option<(Option<SystemTime>, AgentStatus)>,
    path: Option<PathBuf>,
    checked: Instant,
    generation: usize,
    md: MdPane,
    copying: Option<Receiver<Result<String, String>>>,
}

impl OutputPane {
    pub fn new() -> OutputPane {
        OutputPane {
            loaded: None,
            error: None,
            loading: None,
            trigger: None,
            path: None,
            checked: Instant::now(),
            generation: 0,
            md: MdPane::default(),
            copying: None,
        }
    }

    /// The text can be copied (loaded, and no full read is in flight).
    pub fn can_copy(&self) -> bool {
        self.loaded.is_some()
    }

    pub fn copy_label(&self) -> &'static str {
        if self.copying.is_some() {
            "Copying…"
        } else {
            "Copy"
        }
    }

    /// Copy was clicked: the loaded text when it is the whole file, otherwise the full file is
    /// read on a worker and delivered through [`OutputPane::poll`].
    pub fn copy_clicked(&mut self, ctx: &egui::Context) -> Result<Option<String>, String> {
        let Some(loaded) = self.loaded.as_ref() else {
            return Ok(None);
        };
        if !loaded.truncated {
            return Ok(Some(loaded.text.clone()));
        }
        let (Some(path), None) = (self.path.clone(), &self.copying) else {
            return Ok(None);
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("deck-output-copy".into())
            .spawn(move || {
                let r = load(&path, None, None).map(|l| l.map(|l| l.text).unwrap_or_default());
                let _ = tx.send(r);
                ctx.request_repaint();
            })
            .map_err(|e| e.to_string())?;
        self.copying = Some(rx);
        Ok(None)
    }

    /// Collects finished worker results. `Some(result)` = the full-file Copy finished.
    pub fn poll(&mut self) -> Option<Result<String, String>> {
        if let Some(rx) = &self.loading {
            let done = match rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("the reader stopped".to_owned())),
            };
            if let Some(r) = done {
                self.loading = None;
                match r {
                    Ok(Some(l)) => {
                        self.loaded = Some(l);
                        self.error = None;
                        self.generation += 1;
                    }
                    Ok(None) => self.error = None,
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if let Some(rx) = &self.copying {
            let done = match rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("the reader stopped".to_owned())),
            };
            if done.is_some() {
                self.copying = None;
            }
            return done;
        }
        None
    }

    /// Starts a (re)load when the agent's file or activity changed, or the periodic re-check is
    /// due. Only called while the Output tab is on screen.
    fn sync(&mut self, ctx: &egui::Context, a: &AgentSummary) {
        if a.output_path != self.path {
            self.path = a.output_path.clone();
            self.loaded = None;
            self.error = None;
            self.loading = None;
            self.trigger = None;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        ctx.request_repaint_after(RECHECK);
        let trigger = (a.last_activity, a.status);
        let due = self.trigger != Some(trigger) || self.checked.elapsed() >= RECHECK;
        if !due || self.loading.is_some() {
            return;
        }
        self.trigger = Some(trigger);
        self.checked = Instant::now();
        let known = self.loaded.as_ref().map(|l| l.version);
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("deck-output".into())
            .spawn(move || {
                let _ = tx.send(load(&path, Some(PREVIEW_CAP), known));
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.loading = Some(rx),
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, a: &AgentSummary) {
        let pal = Palette::of_ui(ui);
        if a.output_path.is_none() {
            self.sync(ui.ctx(), a);
            let (why, sub) = match a.status {
                AgentStatus::Running => (
                    "Output appears when the agent finishes",
                    "The final answer is written to a file once the agent yields.",
                ),
                AgentStatus::Done => (
                    "This agent left no output file",
                    "It finished without a final text answer.",
                ),
                _ => (
                    "No output",
                    "Failed, aborted and killed agents write no output file.",
                ),
            };
            empty_state(ui, ICON_FILE_TEXT, pal.weak, why, sub);
            return;
        }
        self.sync(ui.ctx(), a);
        let Some(loaded) = &self.loaded else {
            if let Some(message) = &self.error {
                empty_state(
                    ui,
                    ICON_FILE_TEXT,
                    pal.danger,
                    "Cannot read the output file",
                    message,
                );
            } else {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.add(Spin::new(22.0, pal.accent));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Reading output…").color(pal.weak));
                });
            }
            return;
        };
        if loaded.text.trim().is_empty() {
            empty_state(ui, ICON_FILE_TEXT, pal.weak, "The output file is empty", "");
            return;
        }
        if loaded.truncated {
            Frame::NONE
                .fill(pal.warn.gamma_multiply(0.14))
                .stroke(egui::Stroke::new(1.0, pal.warn.gamma_multiply(0.5)))
                .corner_radius(8)
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(icon_text(ICON_INFO, 15.0, pal.warn));
                        ui.label(
                            RichText::new(format!(
                                "Preview truncated: first {} of {}. Copy reads the whole file.",
                                mib(PREVIEW_CAP),
                                mib(loaded.total),
                            ))
                            .size(12.5)
                            .color(pal.warn),
                        );
                    });
                });
            ui.add_space(6.0);
        }
        self.md.show(
            ui,
            Id::unique("agent-output-md"),
            &loaded.text,
            self.generation,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("omp-deck-out-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap_or_else(|e| panic!("write {}: {e}", p.display()));
        p
    }

    #[test]
    fn json_string_payloads_are_decoded() {
        assert_eq!(
            unwrap_json_string("\"line one\\n\\n## Head\"\n".to_owned()),
            "line one\n\n## Head"
        );
    }

    #[test]
    fn other_text_is_left_alone() {
        for t in [
            "plain **markdown**",
            "\"unterminated",
            "\"a\" and \"b\"",
            "\"",
        ] {
            assert_eq!(unwrap_json_string(t.to_owned()), t);
        }
    }

    #[test]
    fn oversized_files_report_truncation_and_full_reads_do_not() {
        let p = temp("big", &[b'x'; 100]);
        let cut = load(&p, Some(40), None).unwrap().unwrap();
        assert!(cut.truncated);
        assert_eq!(cut.text.len(), 40);
        assert_eq!(cut.total, 100);
        let exact = load(&p, Some(100), None).unwrap().unwrap();
        assert!(!exact.truncated);
        let full = load(&p, None, None).unwrap().unwrap();
        assert_eq!(full.text.len(), 100);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn an_unchanged_file_is_not_read_again_but_a_rewrite_is() {
        let p = temp("ver", b"one");
        let first = load(&p, Some(10), None).unwrap().unwrap();
        assert!(load(&p, Some(10), Some(first.version)).unwrap().is_none());
        std::fs::write(&p, b"three").unwrap();
        let again = load(&p, Some(10), Some(first.version)).unwrap().unwrap();
        assert_eq!(again.text, "three");
        let _ = std::fs::remove_file(p);
    }
}

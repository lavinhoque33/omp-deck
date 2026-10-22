//! Serialises config writes on one background thread so the UI never does file I/O.

use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use deck_core::config::AppConfig;

pub struct ConfigWriter {
    tx: Option<Sender<AppConfig>>,
    thread: Option<JoinHandle<()>>,
}

impl ConfigWriter {
    pub fn spawn() -> ConfigWriter {
        let (tx, rx) = mpsc::channel::<AppConfig>();
        let thread = std::thread::Builder::new()
            .name("config-writer".into())
            .spawn(move || {
                for cfg in rx {
                    if let Err(e) = cfg.save() {
                        log::warn!("saving config failed: {e}");
                    }
                }
            })
            .map_err(|e| log::error!("cannot start config writer: {e}"))
            .ok();
        ConfigWriter {
            tx: Some(tx),
            thread,
        }
    }

    pub fn save(&self, cfg: AppConfig) {
        if let Some(tx) = &self.tx
            && tx.send(cfg).is_err()
        {
            log::warn!("config writer is gone");
        }
    }

    /// Send the last config and wait until it is on disk.
    pub fn finish(&mut self, cfg: AppConfig) {
        self.save(cfg);
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ConfigWriter {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

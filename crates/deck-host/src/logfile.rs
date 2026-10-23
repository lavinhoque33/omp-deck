//! Daemon log: `<cache_dir>/omp-deck/host/<ns>/<name>.log`, kept ≤ 1 MiB (truncated and
//! restarted when the next line would exceed the cap).

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG: u64 = 1 << 20;

struct FileLogger {
    file: Mutex<(File, u64)>,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let line = format!("{ms} {} {}\n", record.level(), record.args());
        let mut guard = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let (file, len) = &mut *guard;
        if *len + line.len() as u64 > MAX_LOG {
            if file
                .set_len(0)
                .and_then(|()| file.seek(SeekFrom::Start(0)))
                .is_err()
            {
                return;
            }
            *len = 0;
        }
        if file.write_all(line.as_bytes()).is_ok() {
            *len += line.len() as u64;
        }
    }

    fn flush(&self) {
        let _ = self
            .file
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .flush();
    }
}

/// Installs the file logger (best effort: without a cache directory nothing is logged).
pub(crate) fn init(path: Option<&Path>) {
    let Some(path) = path else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let mut len = file.seek(SeekFrom::End(0)).unwrap_or(0);
    if len > MAX_LOG / 2 {
        // Start each daemon with room to log; the old content is not needed after a restart.
        if file.set_len(0).is_ok() {
            len = 0;
        }
    }
    static LOGGER: OnceLock<FileLogger> = OnceLock::new();
    if LOGGER
        .set(FileLogger {
            file: Mutex::new((file, len)),
        })
        .is_err()
    {
        return;
    }
    if let Some(logger) = LOGGER.get()
        && log::set_logger(logger).is_ok()
    {
        log::set_max_level(log::LevelFilter::Info);
    }
}

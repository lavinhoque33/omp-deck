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

/// Installs the file logger (best effort: without a cache directory nothing is logged). The
/// log may contain session output fragments: Unix keeps it 0600 in 0700 directories.
pub(crate) fn init(path: Option<&Path>) {
    let Some(path) = path else { return };
    if let Some(dir) = path.parent() {
        create_private_dir(dir);
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let Ok(mut file) = options.open(path) else {
        return;
    };
    // Logs created by older versions were 0644.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
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

/// `<cache>/omp-deck/host/<ns>`: missing directories are created 0700; the namespace and
/// `host` directories (ours) are tightened to 0700 if they already existed.
fn create_private_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
        for d in [Some(dir), dir.parent()].into_iter().flatten() {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(dir);
    }
}

//! Unix PTY: `posix_openpt` master, slave opened `O_NOCTTY` (the daemon never acquires it),
//! child started with `setsid` + `TIOCSCTTY` and the slave as stdio, so its process group is
//! the session's process tree.

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{PtySize, Spawned};
use crate::error::{Error, Result};
use crate::protocol::SessionSpec;

fn check(ret: libc::c_int, call: &'static str) -> Result<libc::c_int> {
    if ret < 0 {
        Err(Error::last_os(call))
    } else {
        Ok(ret)
    }
}

fn winsize(size: PtySize) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.cols.saturating_mul(size.cell_width),
        ws_ypixel: size.rows.saturating_mul(size.cell_height),
    }
}

fn set_winsize(fd: &OwnedFd, size: PtySize) -> Result<()> {
    let ws = winsize(size);
    // SAFETY: valid fd and a valid winsize pointer for the duration of the call.
    check(
        unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) },
        "ioctl(TIOCSWINSZ)",
    )?;
    Ok(())
}

/// Slave device path of `master` (thread-safe variant where available).
fn slave_path(master: &OwnedFd) -> Result<String> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut buf = [0 as libc::c_char; 128];
        // SAFETY: buf is writable for its length; master is a valid PTY master fd.
        let rc = unsafe { libc::ptsname_r(master.as_raw_fd(), buf.as_mut_ptr(), buf.len()) };
        if rc != 0 {
            return Err(Error::Os {
                call: "ptsname_r",
                source: io::Error::from_raw_os_error(rc),
            });
        }
        // SAFETY: ptsname_r NUL-terminated the buffer on success.
        Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: master is a valid PTY master fd; the returned static buffer is copied before
        // any other ptsname call (the daemon spawns its one child before starting threads).
        let p = unsafe { libc::ptsname(master.as_raw_fd()) };
        if p.is_null() {
            return Err(Error::last_os("ptsname"));
        }
        // SAFETY: non-null NUL-terminated string from ptsname.
        Ok(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

pub(crate) fn spawn(spec: &SessionSpec, size: PtySize) -> Result<Spawned> {
    // SAFETY: plain syscall; the returned fd is owned below.
    let master = check(
        unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) },
        "posix_openpt",
    )?;
    // SAFETY: freshly opened fd we own exclusively.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: valid PTY master fd.
    check(unsafe { libc::grantpt(master.as_raw_fd()) }, "grantpt")?;
    // SAFETY: valid PTY master fd.
    check(unsafe { libc::unlockpt(master.as_raw_fd()) }, "unlockpt")?;
    let tty = slave_path(&master)?;
    let path = std::ffi::CString::new(tty.clone()).map_err(|e| Error::Os {
        call: "ptsname",
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    // SAFETY: valid C string; the returned fd is owned below.
    let slave = check(
        unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        },
        "open(pty slave)",
    )?;
    // SAFETY: freshly opened fd we own exclusively.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    set_winsize(&master, size)?;

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.env_set {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls (setsid, ioctl) between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // stdin is the slave: make it the controlling terminal of the new session.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|source| Error::Spawn {
        program: spec.program.display().to_string(),
        source,
    })?;
    // Reaped by `PtyChild::wait` (std's Child never waits on drop).
    let pid = child.id();
    drop(cmd);

    let reader = PtyReader(File::from(master.try_clone()?));
    let writer = PtyWriter(File::from(master.try_clone()?));
    Ok(Spawned {
        reader,
        writer,
        child: PtyChild { master, pid, tty },
    })
}

/// PTY output.
pub struct PtyReader(File);

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            // Linux reports EIO once every slave holder is gone: that is end of output.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(0),
            other => other,
        }
    }
}

/// PTY input.
pub struct PtyWriter(File);

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Control handle for the child and its process group.
pub struct PtyChild {
    master: OwnedFd,
    pid: u32,
    tty: String,
}

impl PtyChild {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Slave path, e.g. `/dev/pts/3`.
    pub fn tty(&self) -> Option<&str> {
        Some(&self.tty)
    }

    pub fn resize(&self, size: PtySize) -> Result<()> {
        set_winsize(&self.master, size)
    }

    fn pgid(&self) -> libc::pid_t {
        // The child called setsid: its pid is its process group id. pids fit pid_t.
        -(self.pid as libc::pid_t)
    }

    /// Blocks until the child exits; its exit code (`None` if killed by a signal).
    /// Call from exactly one thread.
    pub fn wait(&self) -> Option<i32> {
        let mut status = 0;
        loop {
            // SAFETY: valid out pointer; pid is our unreaped child.
            let rc = unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, 0) };
            if rc >= 0 {
                break;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return None;
            }
        }
        if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else {
            None
        }
    }

    /// Kills the process tree: `SIGHUP` to the process group, `SIGKILL` to whatever remains
    /// after `grace`. Returns once the group is empty or `SIGKILL` was sent.
    pub fn kill_tree(&self, grace: Duration) {
        let group = self.pgid();
        // SAFETY: plain signal delivery to our child's process group.
        unsafe { libc::kill(group, libc::SIGHUP) };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            // SAFETY: signal 0 only checks for existence.
            if unsafe { libc::kill(group, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: plain signal delivery to our child's process group.
        unsafe { libc::kill(group, libc::SIGKILL) };
    }

    /// Nothing to release early on Unix (the master closes on drop).
    pub fn close_console(&self) {}
}

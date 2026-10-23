//! `CtlClient` against an in-test fake extension server on a real Windows named pipe: the
//! client's polling reads must neither starve its own writes nor lose data, and the pipe name of
//! a tab must be what the client opens.
#![cfg(windows)]

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use deck_core::ctl::{
    CtlClient, CtlCommand, CtlReply, ctl_dir, ctl_env, prepare_ctl_dir, prune_ctl_dirs,
};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// The server end of a one-client pipe called `name`.
fn serve(name: &Path) -> File {
    let wide: Vec<u16> = name.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the null security attributes mean
    // the default descriptor.
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            64 * 1024,
            64 * 1024,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `handle` is a fresh pipe handle this File now owns.
    unsafe { File::from_raw_handle(handle as _) }
}

#[test]
fn a_tab_pipe_serves_snapshots_and_takes_commands_at_the_same_time() {
    let dir = ctl_dir("omp-deck-test-pipe", "omp-pipe-roundtrip");
    prepare_ctl_dir(&dir).unwrap();
    assert_eq!(ctl_env(&dir).0, "OMPDECK_CTL_PIPE");
    let mut server = serve(&dir);
    let client = CtlClient::spawn(dir.clone(), || {});
    // SAFETY: the pipe handle is valid; ConnectNamedPipe blocks until the client opened it.
    let connected = unsafe {
        ConnectNamedPipe(
            std::os::windows::io::AsRawHandle::as_raw_handle(&server) as _,
            std::ptr::null_mut(),
        )
    };
    assert!(connected != 0 || std::io::Error::last_os_error().raw_os_error() == Some(535));

    server
        .write_all(
            b"{\"t\":\"hello\",\"v\":1,\"pid\":4242}\n\
              {\"t\":\"agents\",\"pid\":4242,\"sessionFile\":\"/s/main.jsonl\",\"mainIdle\":true,\"leaf\":\"e7\",\"agents\":[]}\n",
        )
        .unwrap();
    wait_for("snapshot", || client.latest().is_some());
    assert_eq!(client.latest().unwrap().pid, 4242);
    assert!(client.connected());

    // The client's write goes out while its reader is waiting for the next push.
    let rid = client.send(CtlCommand::Kill {
        id: "0-A".into(),
        root: "/s/main.jsonl".into(),
    });
    let mut line = String::new();
    BufReader::new(&mut server).read_line(&mut line).unwrap();
    let cmd: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(cmd["rid"], rid);
    assert_eq!(cmd["t"], "kill");
    server
        .write_all(format!("{{\"t\":\"reply\",\"rid\":{rid},\"ok\":true}}\n").as_bytes())
        .unwrap();
    wait_for("reply", || {
        client
            .take_replies()
            .iter()
            .any(|(r, reply)| *r == rid && matches!(reply, CtlReply { ok: true, .. }))
    });

    // The server going away is a disconnect, not a hang.
    drop(server);
    wait_for("disconnect", || !client.connected());
    prune_ctl_dirs("omp-deck-test-pipe", &Default::default());
}

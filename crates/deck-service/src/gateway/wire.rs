//! Frame codec of the gateway link: `u32 LE length | kind byte | payload`, at most
//! [`MAX_FRAME`] bytes per frame. Control payloads are JSON; terminal bytes and geometry are
//! binary. Large replies and snapshots are split into [`CHUNK`]-sized frames and reassembled under
//! a cap, so no single frame or reassembly grows without bound.

use std::collections::HashMap;
use std::io::{self, Read, Write};

/// Largest frame (kind byte + payload).
pub const MAX_FRAME: usize = 1024 * 1024;
/// Payload bytes per chunk of a chunked message.
pub const CHUNK: usize = 512 * 1024;
/// Largest reassembled reply or snapshot.
pub const MAX_ASSEMBLED: usize = 64 * 1024 * 1024;

/// Writes one frame. Callers serialize writers of one stream (one writer thread or a mutex).
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() + 1;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame of {len} bytes exceeds the {MAX_FRAME} byte limit"),
        ));
    }
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    buf.push(kind);
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Reads one frame; `Ok(None)` at a clean end of stream (between frames).
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match read_full(r, &mut len)? {
        0 => return Ok(None),
        4 => {}
        _ => return Err(io::ErrorKind::UnexpectedEof.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} out of range"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let kind = body[0];
    body.remove(0);
    Ok(Some((kind, body)))
}

/// Reads until `buf` is full or the stream ends; returns the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Header of one chunk: `u64 LE id | u8 last`.
pub const CHUNK_HEADER: usize = 9;

/// Splits `bytes` into chunk payloads (header + slice) for message `id`. An empty message is one
/// empty last chunk.
pub fn chunks(id: u64, bytes: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    let count = bytes.len().div_ceil(CHUNK).max(1);
    (0..count).map(move |i| {
        let part = &bytes[(i * CHUNK).min(bytes.len())..((i + 1) * CHUNK).min(bytes.len())];
        let mut out = Vec::with_capacity(CHUNK_HEADER + part.len());
        out.extend_from_slice(&id.to_le_bytes());
        out.push(u8::from(i + 1 == count));
        out.extend_from_slice(part);
        out
    })
}

/// Reassembles chunked messages by id.
#[derive(Default)]
pub struct Assembly {
    partial: HashMap<u64, Vec<u8>>,
    held: usize,
}

impl Assembly {
    /// Adds one chunk payload; returns the complete message when this was its last chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Option<(u64, Vec<u8>)>, io::Error> {
        if chunk.len() < CHUNK_HEADER {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short chunk"));
        }
        let id = u64::from_le_bytes(chunk[..8].try_into().expect("8 bytes"));
        let last = chunk[8] != 0;
        let data = &chunk[CHUNK_HEADER..];
        if self.held + data.len() > MAX_ASSEMBLED {
            self.held -= self.partial.remove(&id).map_or(0, |v| v.len());
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                TooLargeError(id),
            ));
        }
        self.held += data.len();
        let buf = self.partial.entry(id).or_default();
        buf.extend_from_slice(data);
        if !last {
            return Ok(None);
        }
        let full = self.partial.remove(&id).unwrap_or_default();
        self.held -= full.len();
        Ok(Some((id, full)))
    }
}

#[derive(Debug)]
struct TooLargeError(u64);

impl std::fmt::Display for TooLargeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "message {} exceeds {MAX_ASSEMBLED} bytes", self.0)
    }
}

impl std::error::Error for TooLargeError {}

/// `true` when `e` is a reassembly-cap refusal.
#[cfg(test)]
fn is_too_large(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<TooLargeError>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_oversize_is_refused_both_ways() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 7, b"hello").expect("write");
        write_frame(&mut buf, 8, b"").expect("write");
        let mut r = io::Cursor::new(buf);
        assert_eq!(
            read_frame(&mut r).expect("read"),
            Some((7, b"hello".to_vec()))
        );
        assert_eq!(read_frame(&mut r).expect("read"), Some((8, Vec::new())));
        assert_eq!(read_frame(&mut r).expect("eof"), None);

        assert!(write_frame(&mut Vec::new(), 1, &vec![0; MAX_FRAME]).is_err());
        let mut forged = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        forged.push(1);
        assert!(read_frame(&mut io::Cursor::new(forged)).is_err());
        // A frame cut in the middle is an error, not a clean end.
        let mut cut = Vec::new();
        write_frame(&mut cut, 1, b"abcdef").expect("write");
        cut.truncate(7);
        assert!(read_frame(&mut io::Cursor::new(cut)).is_err());
    }

    #[test]
    fn chunked_messages_reassemble_interleaved_and_respect_the_cap() {
        let big: Vec<u8> = (0..(CHUNK * 2 + 17)).map(|i| i as u8).collect();
        let small = b"x".to_vec();
        let a: Vec<_> = chunks(1, &big).collect();
        let b: Vec<_> = chunks(2, &small).collect();
        assert_eq!(a.len(), 3);
        assert_eq!(chunks(3, &[]).count(), 1);
        let mut asm = Assembly::default();
        assert_eq!(asm.push(&a[0]).expect("chunk"), None);
        assert_eq!(asm.push(&b[0]).expect("chunk"), Some((2, small)));
        assert_eq!(asm.push(&a[1]).expect("chunk"), None);
        assert_eq!(asm.push(&a[2]).expect("chunk"), Some((1, big)));
        assert_eq!(asm.held, 0);

        let mut asm = Assembly::default();
        let mut huge = vec![0u8; CHUNK_HEADER];
        huge[..8].copy_from_slice(&9u64.to_le_bytes());
        huge.extend(std::iter::repeat_n(0u8, CHUNK));
        let mut refused = None;
        for _ in 0..(MAX_ASSEMBLED / CHUNK + 1) {
            if let Err(e) = asm.push(&huge) {
                refused = Some(e);
                break;
            }
        }
        assert!(refused.is_some_and(|e| is_too_large(&e)));
        assert_eq!(asm.held, 0, "the refused message is forgotten");
    }
}

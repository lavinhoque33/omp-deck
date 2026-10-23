//! Windows process-creation helpers shared by the daemon launcher and the ConPTY child.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
};

use crate::error::{Error, Result};

/// Takes ownership of a handle returned by a Win32 call (null/invalid → last OS error).
pub(crate) fn owned(handle: HANDLE, call: &'static str) -> Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(Error::last_os(call));
    }
    // SAFETY: a valid handle we own exclusively from here on.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Maps a Win32 `BOOL` result to the last OS error.
pub(crate) fn check(ok: i32, call: &'static str) -> Result<()> {
    if ok == 0 {
        Err(Error::last_os(call))
    } else {
        Ok(())
    }
}

/// An initialized `PROC_THREAD_ATTRIBUTE_LIST`. Attribute values are referenced, not copied:
/// they must outlive the `CreateProcessW` call that uses the list.
pub(crate) struct AttributeList {
    /// u64 elements keep the opaque list pointer-aligned.
    buf: Vec<u64>,
}

impl AttributeList {
    pub(crate) fn new(count: u32) -> Result<AttributeList> {
        let mut size = 0usize;
        // SAFETY: size query with a null list; fails with ERROR_INSUFFICIENT_BUFFER by design.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
        let mut list = AttributeList {
            buf: vec![0u64; size.div_ceil(8)],
        };
        // SAFETY: buf holds `size` bytes.
        check(
            unsafe { InitializeProcThreadAttributeList(list.as_ptr(), count, 0, &mut size) },
            "InitializeProcThreadAttributeList",
        )
        .inspect_err(|_| list.buf.clear())?;
        Ok(list)
    }

    pub(crate) fn as_ptr(&mut self) -> *mut c_void {
        self.buf.as_mut_ptr().cast()
    }

    /// Sets `attribute` to the `size` bytes at `value`.
    ///
    /// # Safety
    /// `value` must stay valid until the list is no longer used by `CreateProcessW`.
    pub(crate) unsafe fn set(
        &mut self,
        attribute: u32,
        value: *const c_void,
        size: usize,
    ) -> Result<()> {
        // SAFETY: initialized list; the caller keeps `value` alive.
        check(
            unsafe {
                UpdateProcThreadAttribute(
                    self.as_ptr(),
                    0,
                    attribute as usize,
                    value,
                    size,
                    null_mut(),
                    null(),
                )
            },
            "UpdateProcThreadAttribute",
        )
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            // SAFETY: initialized list, deleted once before its buffer is freed.
            unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
        }
    }
}

/// `io::Error` view for callers returning `io::Result`.
pub(crate) fn io_error(e: Error) -> io::Error {
    match e {
        Error::Os { source, .. } | Error::Io(source) => source,
        other => io::Error::other(other.to_string()),
    }
}

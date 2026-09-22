//! Socket tuning not exposed by std.

use std::net::UdpSocket;
use std::os::fd::AsRawFd;

unsafe extern "C" {
    fn setsockopt(fd: i32, level: i32, name: i32, val: *const core::ffi::c_void, len: u32) -> i32;
    fn getsockopt(fd: i32, level: i32, name: i32, val: *mut core::ffi::c_void, len: *mut u32) -> i32;
}

const SOL_SOCKET: i32 = 1;
const SO_RCVBUF: i32 = 8;

/// Asks for a larger receive buffer (capped by net.core.rmem_max) so bursts from an
/// upstream that flushes its queue don't overflow the socket. Returns the size the
/// kernel reports (it doubles the request for bookkeeping), or 0 on failure.
pub fn set_rcvbuf(s: &UdpSocket, bytes: i32) -> i32 {
    let fd = s.as_raw_fd();
    // SAFETY: valid fd, pointer to a live i32 with the correct length.
    unsafe {
        let v = bytes;
        let _ = setsockopt(fd, SOL_SOCKET, SO_RCVBUF, (&v as *const i32).cast(), 4);
        let mut got: i32 = 0;
        let mut len: u32 = 4;
        if getsockopt(fd, SOL_SOCKET, SO_RCVBUF, (&mut got as *mut i32).cast(), &mut len) != 0 {
            return 0;
        }
        got
    }
}

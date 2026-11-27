//! The UDP socket's kernel buffers (SO_RCVBUF / SO_SNDBUF), which std does not expose. Every
//! datagram of every room and call queues in the ONE socket's receive buffer while the network
//! thread works; the defaults (64 KiB on Windows, ~208 KiB on Linux) overflow when a few
//! publishers' keyframes arrive together, and a datagram lost there is a frame lost for every
//! subscriber. A few lines of FFI instead of a crate (socket2) for two calls.
//!
//! Linux caps what is asked at net.core.rmem_max / wmem_max (README: the sysctl to raise them)
//! and reports back twice the value it keeps; the effective sizes are logged at start.

use std::net::UdpSocket;

/// (receive, send) as the kernel reports them after asking for `bytes` each.
pub(crate) fn set(socket: &UdpSocket, bytes: usize) -> (usize, usize) {
    let bytes = bytes.min(i32::MAX as usize) as i32;
    sys::set(socket, sys::SO_RCVBUF, bytes);
    sys::set(socket, sys::SO_SNDBUF, bytes);
    (sys::get(socket, sys::SO_RCVBUF), sys::get(socket, sys::SO_SNDBUF))
}

#[cfg(windows)]
mod sys {
    use std::net::UdpSocket;
    use std::os::windows::io::AsRawSocket;

    pub const SOL_SOCKET: i32 = 0xffff;
    pub const SO_SNDBUF: i32 = 0x1001;
    pub const SO_RCVBUF: i32 = 0x1002;

    #[link(name = "ws2_32")]
    extern "system" {
        fn setsockopt(s: usize, level: i32, name: i32, value: *const i32, len: i32) -> i32;
        fn getsockopt(s: usize, level: i32, name: i32, value: *mut i32, len: *mut i32) -> i32;
    }

    pub fn set(socket: &UdpSocket, name: i32, value: i32) {
        // SAFETY: a live socket of ours, a pointer to an i32 and its size.
        unsafe { setsockopt(socket.as_raw_socket() as usize, SOL_SOCKET, name, &value, 4) };
    }

    pub fn get(socket: &UdpSocket, name: i32) -> usize {
        let (mut value, mut len) = (0i32, 4i32);
        // SAFETY: as above, with room for the i32 the option is.
        let ok = unsafe { getsockopt(socket.as_raw_socket() as usize, SOL_SOCKET, name, &mut value, &mut len) } == 0;
        if ok { value.max(0) as usize } else { 0 }
    }
}

#[cfg(unix)]
mod sys {
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod consts {
        pub const SOL_SOCKET: i32 = 1;
        pub const SO_SNDBUF: i32 = 7;
        pub const SO_RCVBUF: i32 = 8;
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    mod consts {
        // The BSDs and macOS.
        pub const SOL_SOCKET: i32 = 0xffff;
        pub const SO_SNDBUF: i32 = 0x1001;
        pub const SO_RCVBUF: i32 = 0x1002;
    }
    pub use consts::*;

    extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const i32, len: u32) -> i32;
        fn getsockopt(fd: i32, level: i32, name: i32, value: *mut i32, len: *mut u32) -> i32;
    }

    pub fn set(socket: &UdpSocket, name: i32, value: i32) {
        // SAFETY: a live socket of ours, a pointer to an int and its size.
        unsafe { setsockopt(socket.as_raw_fd(), SOL_SOCKET, name, &value, 4) };
    }

    pub fn get(socket: &UdpSocket, name: i32) -> usize {
        let (mut value, mut len) = (0i32, 4u32);
        // SAFETY: as above, with room for the int the option is.
        let ok = unsafe { getsockopt(socket.as_raw_fd(), SOL_SOCKET, name, &mut value, &mut len) } == 0;
        if ok { value.max(0) as usize } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_buffers_grow() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let before = (super::sys::get(&socket, super::sys::SO_RCVBUF), super::sys::get(&socket, super::sys::SO_SNDBUF));
        let (rcv, snd) = super::set(&socket, 1 << 20);
        assert!(rcv > before.0 || rcv >= 1 << 20, "receive {before:?} -> {rcv}");
        assert!(snd >= before.1, "send {before:?} -> {snd}");
    }
}

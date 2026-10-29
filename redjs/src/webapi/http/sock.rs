//! A non-blocking TCP socket with a readiness wait: the blocking transport's
//! only way of waiting on the network.
//!
//! ## Why not a socket timeout
//!
//! Until m26 every socket carried `SO_RCVTIMEO`/`SO_SNDTIMEO` of [`POLL`] and
//! every read retried in a loop. On Windows a blocking `recv` that hits
//! `SO_RCVTIMEO` is an overlapped operation that Winsock cancels, and MSDN
//! states the socket is then in an *indeterminate state* — data arriving
//! around the cancel is not guaranteed to be delivered. Whether or not that
//! race ever bit us (research/reports/m26/net-h2-pool.md §2 has the
//! measurement), a transport whose correctness rests on a documented
//! "indeterminate" is the wrong foundation; and the TLS layer above treats a
//! timed-out read (`TimedOut`) differently from `WouldBlock` mid-handshake.
//!
//! So the socket is put in non-blocking mode and waits happen OUTSIDE the
//! read: `recv` either returns bytes immediately or `WouldBlock`, and only then
//! does the thread sleep in `WSAPoll`/`poll` until the socket is readable (or a
//! slice elapses, to look at the abort flag). Nothing is ever cancelled
//! mid-receive; a byte is either in the kernel buffer or in ours.
//!
//! ## Two modes
//!
//! - [`Wait::Block`] (fetch): a read or write waits until it can make progress,
//!   the request is aborted, or nothing has moved for the stall budget. The
//!   TLS layer above never sees `WouldBlock`, so a native-tls handshake runs in
//!   one call and is still abortable.
//! - [`Wait::Slice`] (WebSocket's loop): wait at most one slice, then return
//!   `WouldBlock` so the caller can flush what JS queued. `WouldBlock` is the
//!   standard non-blocking contract, which the TLS layer resumes correctly.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::{Cancel, POLL};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wait {
    /// Wait for progress; give up only on abort or after `budget` of silence.
    Block { budget: Duration },
    /// Wait at most this long, then report `WouldBlock`.
    Slice(Duration),
}

pub(crate) struct Polled {
    tcp: TcpStream,
    cancel: Cancel,
    wait: Wait,
}

pub(crate) fn aborted_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the request was aborted")
}

pub(crate) fn stalled_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "the connection stalled")
}

impl Polled {
    /// Takes a connected socket and switches it to non-blocking mode.
    pub(crate) fn new(tcp: TcpStream, cancel: Cancel, wait: Wait) -> io::Result<Polled> {
        tcp.set_nonblocking(true)?;
        Ok(Polled { tcp, cancel, wait })
    }

    pub(crate) fn tcp(&self) -> &TcpStream {
        &self.tcp
    }

    pub(crate) fn set_wait(&mut self, wait: Wait) {
        self.wait = wait;
    }

    /// The slice a blocking wait is cut into, so the abort flag is looked at.
    /// `abort` also shuts the socket down, which ends the wait at once; the
    /// slice is the backstop.
    fn slice() -> Duration {
        #[cfg(test)]
        {
            let ms = super::TEST_POLL_MS.load(std::sync::atomic::Ordering::SeqCst);
            if ms != 0 {
                return Duration::from_millis(ms);
            }
        }
        POLL
    }

    fn io(&mut self, write: bool, mut op: impl FnMut(&mut TcpStream) -> io::Result<usize>) -> io::Result<usize> {
        let started = Instant::now();
        loop {
            match op(&mut self.tcp) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    if self.cancel.is_aborted() {
                        return Err(aborted_error());
                    }
                    return Err(e);
                }
            }
            match self.wait {
                Wait::Slice(d) => {
                    // One wait, then hand control back. A second attempt only
                    // if the socket said it is ready.
                    if started.elapsed() >= d || !wait_ready(&self.tcp, write, d)? {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    match op(&mut self.tcp) {
                        Ok(n) => return Ok(n),
                        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                            return Err(io::ErrorKind::WouldBlock.into())
                        }
                        Err(e) => return Err(e),
                    }
                }
                Wait::Block { budget } => {
                    if self.cancel.is_aborted() {
                        return Err(aborted_error());
                    }
                    let left = budget.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        return Err(stalled_error());
                    }
                    wait_ready(&self.tcp, write, left.min(Self::slice()))?;
                    if self.cancel.is_aborted() {
                        return Err(aborted_error());
                    }
                }
            }
        }
    }
}

impl Read for Polled {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io(false, |s| s.read(buf))
    }
}

impl Write for Polled {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io(true, |s| s.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Sleeps until the socket is readable (or writable), errored or hung up —
/// all of which mean "try the operation again" — or `timeout` passes.
/// `Ok(false)` is the timeout.
pub(crate) fn wait_ready(tcp: &TcpStream, write: bool, timeout: Duration) -> io::Result<bool> {
    let ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    sys::poll_one(tcp, write, ms)
}

#[cfg(windows)]
mod sys {
    use std::io;
    use std::net::TcpStream;
    use std::os::windows::io::AsRawSocket;

    #[repr(C)]
    struct WsaPollFd {
        fd: usize,
        events: i16,
        revents: i16,
    }

    const POLLRDNORM: i16 = 0x0100;
    const POLLWRNORM: i16 = 0x0010;

    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAPoll(fds: *mut WsaPollFd, n: u32, timeout: i32) -> i32;
        fn WSAGetLastError() -> i32;
    }

    pub(super) fn poll_one(tcp: &TcpStream, write: bool, ms: i32) -> io::Result<bool> {
        let mut fd = WsaPollFd {
            fd: tcp.as_raw_socket() as usize,
            events: if write { POLLWRNORM } else { POLLRDNORM },
            revents: 0,
        };
        // SAFETY: one valid WSAPOLLFD on the stack, count 1.
        let r = unsafe { WSAPoll(&mut fd, 1, ms) };
        if r < 0 {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        // Any revents (data, POLLHUP, POLLERR) means the next operation will
        // not block: it returns the bytes, the EOF or the error.
        Ok(r > 0)
    }
}

#[cfg(unix)]
mod sys {
    use std::io;
    use std::net::TcpStream;
    use std::os::unix::io::AsRawFd;

    pub(super) fn poll_one(tcp: &TcpStream, write: bool, ms: i32) -> io::Result<bool> {
        let mut fd = libc::pollfd {
            fd: tcp.as_raw_fd(),
            events: if write { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        };
        loop {
            // SAFETY: one valid pollfd on the stack, count 1.
            let r = unsafe { libc::poll(&mut fd, 1, ms) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(r > 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocking_read_waits_for_late_bytes_without_losing_any() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            for i in 0..20u8 {
                std::thread::sleep(Duration::from_millis(37));
                s.write_all(&[i; 3]).unwrap();
            }
        });
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut p = Polled::new(tcp, Cancel::default(), Wait::Block { budget: Duration::from_secs(5) }).unwrap();
        let mut got = Vec::new();
        p.read_to_end(&mut got).unwrap();
        let want: Vec<u8> = (0..20u8).flat_map(|i| [i; 3]).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn silence_past_the_budget_is_a_stall_not_a_hang() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let held = l.accept();
            std::thread::sleep(Duration::from_secs(5));
            drop(held);
        });
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut p = Polled::new(tcp, Cancel::default(), Wait::Block { budget: Duration::from_millis(300) }).unwrap();
        let t0 = Instant::now();
        let e = p.read(&mut [0u8; 8]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    }

    #[test]
    fn a_slice_wait_hands_control_back_with_would_block() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let held = l.accept();
            std::thread::sleep(Duration::from_secs(2));
            drop(held);
        });
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut p = Polled::new(tcp, Cancel::default(), Wait::Slice(Duration::from_millis(5))).unwrap();
        let t0 = Instant::now();
        let e = p.read(&mut [0u8; 8]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert!(t0.elapsed() < Duration::from_millis(500), "{:?}", t0.elapsed());
    }
}

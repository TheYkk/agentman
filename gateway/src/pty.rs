//! Minimal host-side pseudo-terminal support for attaching to LXC containers.
//!
//! `lxc-attach` allocates a terminal inside the container when its own stdio is a TTY, and
//! forwards window size changes (SIGWINCH) to it. So for interactive sessions we run
//! `lxc-attach` on the slave side of a host PTY and bridge the master side to the SSH channel.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use anyhow::{anyhow, Result};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Master side of a PTY, registered with the tokio reactor.
pub struct PtyMaster {
    fd: AsyncFd<OwnedFd>,
}

/// Open a PTY pair with the given initial size. Returns (master, slave).
pub fn open_pty(cols: u16, rows: u16) -> Result<(PtyMaster, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: openpty writes two fds into the provided out-params; name/termios may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if rc != 0 {
        return Err(anyhow!("openpty failed: {}", io::Error::last_os_error()));
    }

    // SAFETY: openpty succeeded, so both fds are valid and owned by us.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };

    set_cloexec(&master)?;
    set_cloexec(&slave)?;
    set_nonblocking(&master)?;

    Ok((
        PtyMaster {
            fd: AsyncFd::new(master)?,
        },
        slave,
    ))
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fcntl on a valid fd.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fcntl on a valid fd.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Make the slave PTY (already dup'd onto stdin) the controlling terminal of a new session.
///
/// Intended for `Command::pre_exec`: only async-signal-safe calls.
pub fn make_controlling_terminal() -> io::Result<()> {
    // SAFETY: setsid/ioctl are async-signal-safe and act on the current (child) process.
    unsafe {
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl PtyMaster {
    /// Set the terminal size; the kernel delivers SIGWINCH to the foreground process group.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads a winsize struct from a valid pointer.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        if rc < 0 {
            return Err(anyhow!(
                "TIOCSWINSZ failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn poll_read_buf(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            // SAFETY: reading into an initialized, writable buffer of the given length.
            let res = guard.try_io(|inner| {
                let n = unsafe {
                    libc::read(
                        inner.as_raw_fd(),
                        unfilled.as_mut_ptr().cast(),
                        unfilled.len(),
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                // Linux returns EIO on the master once every slave fd is closed: that's EOF.
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Poll::Ready(Ok(())),
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_write_buf(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.fd.poll_write_ready(cx))?;
            // SAFETY: writing from a valid buffer of the given length.
            let res = guard.try_io(|inner| {
                let n = unsafe { libc::write(inner.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(r) => return Poll::Ready(r),
                Err(_would_block) => continue,
            }
        }
    }
}

/// Reading half of a shared [`PtyMaster`].
pub struct PtyReader(pub Arc<PtyMaster>);

/// Writing half of a shared [`PtyMaster`].
pub struct PtyWriter(pub Arc<PtyMaster>);

impl AsyncRead for PtyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.0.poll_read_buf(cx, buf)
    }
}

impl AsyncWrite for PtyWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.poll_write_buf(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn pty_roundtrip_sees_tty_and_size() {
        let (master, slave) = open_pty(100, 30).unwrap();
        let master = Arc::new(master);

        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("read line; test -t 0 && echo tty=yes; stty size; echo got=$line")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: pre_exec runs only async-signal-safe calls.
        unsafe {
            cmd.pre_exec(make_controlling_terminal);
        }
        let mut child = cmd.spawn().unwrap();
        drop(cmd);

        let mut writer = PtyWriter(master.clone());
        writer.write_all(b"hello\n").await.unwrap();

        let mut out = Vec::new();
        PtyReader(master).read_to_end(&mut out).await.unwrap();
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains("tty=yes"), "{out}");
        assert!(out.contains("30 100"), "{out}");
        assert!(out.contains("got=hello"), "{out}");
        assert!(child.wait().await.unwrap().success());
    }
}

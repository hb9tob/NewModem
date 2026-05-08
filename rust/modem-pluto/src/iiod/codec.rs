//! Wire-protocol primitives shared by all iiod commands.
//!
//! iiod's wire format is small and uniform:
//!
//! - **Request**: an ASCII command line, `\n`-terminated. For commands
//!   that carry binary payload (`WRITEBUF`), the payload follows the
//!   newline immediately, with the byte count embedded in the command.
//! - **Response**: a *status line* (`%li\n` — signed long decimal).
//!     - **Negative** → `-errno`. The connection stays alive; the
//!       caller treats it as a per-op failure.
//!     - **Zero or positive** → byte count of payload that follows.
//!       For attribute reads that's the value bytes (libiio includes
//!       the trailing `\0` in the count). For `READBUF` it's the
//!       chunk size. The wire then carries exactly that many bytes,
//!       followed by a single `\n` (libiio's `write_all(buf, ret + 1)`
//!       convention).
//!
//! This module concentrates the read/write helpers so the higher-level
//! commands don't have to repeat the line-and-trailing-newline dance.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use crate::iiod::error::IiodError;

/// Wrapper around `BufReader<TcpStream>` plus the unbuffered write
/// half (we explicitly avoid buffering writes — iiod is a request /
/// response protocol where holding bytes would only add latency).
///
/// Uses `try_clone` to get two handles on the same underlying socket;
/// reads go through the buffered side, writes go straight to the
/// unbuffered handle. Closing either drops the socket.
pub struct IiodTransport {
    pub(crate) reader: BufReader<TcpStream>,
    pub(crate) writer: TcpStream,
}

impl IiodTransport {
    /// Wrap an opened TCP connection into a transport, with an 8 KB
    /// read buffer (large enough to hold the typical attribute
    /// reply in one syscall, small enough to not waste RAM when we
    /// open three connections for control / RX / TX).
    pub fn new(stream: TcpStream) -> Result<Self, IiodError> {
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::with_capacity(8 * 1024, stream),
            writer,
        })
    }

    /// Send a complete command line. Adds the trailing `\n` for the
    /// caller — passing `"VERSION"` here writes exactly `b"VERSION\n"`.
    /// Caller is responsible for not embedding a literal `\n` in
    /// `cmd` (the iiod parser would treat that as command boundary).
    pub fn send_line(&mut self, cmd: &str) -> Result<(), IiodError> {
        debug_assert!(
            !cmd.contains('\n'),
            "iiod command line must not embed newlines: {cmd:?}"
        );
        self.writer.write_all(cmd.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send raw bytes (used by `WRITE` / `WRITEBUF` to push payload
    /// after the command line). No framing is added.
    pub fn send_bytes(&mut self, data: &[u8]) -> Result<(), IiodError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Read one `\n`-terminated line. Returns the line **without**
    /// the trailing `\n`. Empty (just a `\n`) is allowed and surfaces
    /// as `Ok("")`.
    pub fn recv_line(&mut self) -> Result<String, IiodError> {
        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(IiodError::Protocol(
                "iiod closed connection before sending a line".into(),
            ));
        }
        // `read_line` keeps the trailing `\n` (and possibly `\r\n` on
        // some servers — iiod itself sends `\n` only, but tolerate
        // both to be liberal in what we accept).
        if buf.ends_with('\n') {
            buf.pop();
        }
        if buf.ends_with('\r') {
            buf.pop();
        }
        Ok(buf)
    }

    /// Read exactly `n` bytes into a fresh `Vec`. Used for binary
    /// payloads (attribute values, READBUF chunks).
    pub fn recv_exact(&mut self, n: usize) -> Result<Vec<u8>, IiodError> {
        let mut buf = vec![0u8; n];
        self.reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read `dest.len()` bytes straight into the caller's slice. Hot
    /// path for `READBUF` — avoids the `Vec` allocation on every
    /// chunk and lets the caller reuse one I/Q scratch buffer
    /// across the whole streaming session.
    pub fn recv_exact_into(&mut self, dest: &mut [u8]) -> Result<(), IiodError> {
        self.reader.read_exact(dest)?;
        Ok(())
    }

    /// Drop a single trailing `\n` (libiio appends one after every
    /// payload — see the `write_all(buf, ret + 1)` convention in
    /// `iiod/ops.c`). On Pluto firmware this byte is sometimes
    /// absent (older builds use `ret` instead of `ret + 1`); we
    /// peek-then-consume so we don't desync the stream when it's
    /// missing.
    pub fn consume_trailing_newline(&mut self) -> Result<(), IiodError> {
        // `fill_buf` blocks waiting for at least one byte. We can't
        // do a non-blocking peek without temporarily flipping the
        // socket non-blocking, which is more trouble than it's worth
        // — in practice the trailing `\n` is sent immediately after
        // the payload bytes by `write_all` in the same syscall on
        // the server, so it's always already in our BufReader's
        // buffer by the time we get here.
        let avail = self.reader.fill_buf()?;
        if avail.first() == Some(&b'\n') {
            self.reader.consume(1);
        }
        // If the next byte isn't `\n`, leave it in the buffer for
        // the next command — better tolerant than strict.
        Ok(())
    }
}

/// Parse a status line of the form `%li\n` (already stripped of the
/// trailing `\n` by `recv_line`). Returns the integer; the caller
/// decides whether negative is acceptable.
pub fn parse_status(line: &str, ctx: &str) -> Result<i64, IiodError> {
    line.trim().parse::<i64>().map_err(|_| {
        IiodError::BadStatusLine(format!("{ctx}: expected integer, got {line:?}"))
    })
}

/// Convert a server status into a `Result`: positive / zero is the
/// payload size, negative is converted into `IiodError::ServerErrno`.
pub fn status_to_size(status: i64, context: &str) -> Result<usize, IiodError> {
    if status < 0 {
        Err(IiodError::ServerErrno {
            errno: (-status) as i32,
            context: context.to_string(),
        })
    } else {
        Ok(status as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_positive_status() {
        assert_eq!(parse_status("0", "test").unwrap(), 0);
        assert_eq!(parse_status("42", "test").unwrap(), 42);
        assert_eq!(parse_status("9999999", "test").unwrap(), 9_999_999);
    }

    #[test]
    fn parses_negative_status() {
        // -22 is EINVAL in the iiod convention — exactly the error
        // we'll see when writing an out-of-range freq, so treat it
        // as the canonical regression case.
        assert_eq!(parse_status("-22", "test").unwrap(), -22);
        assert_eq!(parse_status("-1", "test").unwrap(), -1);
    }

    #[test]
    fn tolerates_whitespace_around_status() {
        assert_eq!(parse_status("  0  ", "test").unwrap(), 0);
        assert_eq!(parse_status("\t-22\t", "test").unwrap(), -22);
    }

    #[test]
    fn rejects_non_integer_status() {
        let err = parse_status("OK", "VERSION").unwrap_err();
        assert!(matches!(err, IiodError::BadStatusLine(_)));
        let err = parse_status("", "VERSION").unwrap_err();
        assert!(matches!(err, IiodError::BadStatusLine(_)));
    }

    #[test]
    fn status_to_size_maps_negative_to_errno() {
        let err = status_to_size(-22, "WRITE rx_lo").unwrap_err();
        match err {
            IiodError::ServerErrno { errno, context } => {
                assert_eq!(errno, 22);
                assert!(context.contains("rx_lo"));
            }
            other => panic!("expected ServerErrno, got {other:?}"),
        }
        assert_eq!(status_to_size(0, "x").unwrap(), 0);
        assert_eq!(status_to_size(123, "x").unwrap(), 123);
    }
}

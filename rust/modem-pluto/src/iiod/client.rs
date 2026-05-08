//! High-level iiod client built on top of [`crate::iiod::codec`].
//!
//! This file holds the `IiodClient` type the rest of `modem-pluto`
//! uses. Today it covers connection setup + the *control-plane* slice
//! of the protocol (VERSION, PRINT, READ/WRITE attrs, TIMEOUT, EXIT)
//! — exactly what `device::open` needs to program the AD9361. The
//! buffer-streaming commands (OPEN, CLOSE, READBUF, WRITEBUF) land in
//! a follow-up commit; that's where we'll wire `rx::start` and
//! `tx::start` over to the new transport.
//!
//! ## Why split the work
//!
//! The control plane is short, easy to test interactively (see
//! `examples/probe_iiod.rs`), and gives us a fast smoke test of the
//! whole transport — handshake, framing, status decoding. Once that
//! lights up against a real Pluto, the streaming side adds purely
//! incrementally and we can move forward without re-debugging the
//! framing layer underneath.

use std::net::TcpStream;
use std::time::Duration;

use crate::iiod::codec::{parse_status, status_to_size, IiodTransport};
use crate::iiod::error::IiodError;
use crate::iiod::target::{parse_pluto_target, PlutoTarget};

/// Direction of an IIO channel attribute. Maps to the wire tokens
/// `INPUT` and `OUTPUT` in the iiod grammar (`parser.y`).
///
/// Mirrors `industrial_io::Direction` so the porting in `device.rs`
/// stays a one-to-one rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChanDir {
    Input,
    Output,
}

impl ChanDir {
    /// Wire token (`"INPUT"` or `"OUTPUT"`).
    pub fn as_iiod_token(self) -> &'static str {
        match self {
            ChanDir::Input => "INPUT",
            ChanDir::Output => "OUTPUT",
        }
    }
}

/// Server version reported by `VERSION`, parsed from
/// `MAJOR.MINOR.GIT-7chars\n`. We keep the git tag as-is because Pluto
/// firmwares pin specific commits and that string is sometimes the
/// only way to disambiguate buggy driver builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerVersion {
    pub major: u16,
    pub minor: u16,
    /// 7-character git short hash (`tezuka` Pluto firmwares emit a
    /// human-readable tag here, e.g. `tezuka-`).
    pub git: String,
}

impl ServerVersion {
    /// Parse a version line — anything we can't decode falls back to
    /// `(0, 0, raw_string)` so a stray Pluto firmware variant doesn't
    /// fail the connect step. The version is informational.
    fn parse(raw: &str) -> Self {
        let parts: Vec<&str> = raw.splitn(3, '.').collect();
        match parts.as_slice() {
            [maj, min, rest] => {
                let major = maj.trim().parse::<u16>().unwrap_or(0);
                let minor = min.trim().parse::<u16>().unwrap_or(0);
                Self {
                    major,
                    minor,
                    git: rest.trim().to_string(),
                }
            }
            _ => Self {
                major: 0,
                minor: 0,
                git: raw.to_string(),
            },
        }
    }
}

/// One TCP connection to an iiod server.
///
/// Single-threaded by construction (`!Sync`). To run RX and TX
/// concurrently, open a second `IiodClient` against the same target
/// — iiod allocates one server thread per client, the AD9361
/// hardware itself serializes contention.
pub struct IiodClient {
    transport: IiodTransport,
    target: PlutoTarget,
    server_version: ServerVersion,
}

impl IiodClient {
    /// Open a TCP connection and perform the `VERSION` handshake.
    ///
    /// `target` accepts everything [`parse_pluto_target`] does:
    /// `"ip:pluto.local"`, `"192.168.2.1"`, `"192.168.10.50:31000"`,
    /// IPv6 with brackets, etc. — the GUI's free-text host field
    /// pipes straight here.
    pub fn connect(target: &str) -> Result<Self, IiodError> {
        let parsed = parse_pluto_target(target)?;
        let addr = parsed.socket_addr_str();

        let stream = TcpStream::connect(&addr).map_err(|e| IiodError::Resolve {
            host: parsed.host.clone(),
            source: e,
        })?;
        // 5-second default — control-plane attrs return in <50 ms,
        // but mDNS resolution behind the scenes can stall for a
        // second on first connect. Streaming connections override
        // this with a longer timeout when they're set up.
        let read_timeout = Duration::from_secs(5);
        let write_timeout = Duration::from_secs(5);
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(write_timeout))?;
        // Disable Nagle — iiod is a strict request/response chat
        // and we want each command line on the wire immediately.
        stream.set_nodelay(true)?;

        let mut transport = IiodTransport::new(stream)?;

        // Handshake: ask for version. Pluto's iiod replies with one
        // line of the form `0.25.tezuka-` or similar.
        transport.send_line("VERSION")?;
        let version_line = transport.recv_line()?;
        let server_version = ServerVersion::parse(&version_line);

        Ok(Self {
            transport,
            target: parsed,
            server_version,
        })
    }

    /// The target this client is connected to (for log messages /
    /// reconnect logic).
    pub fn target(&self) -> &PlutoTarget {
        &self.target
    }

    /// Server version reported during the handshake.
    pub fn server_version(&self) -> &ServerVersion {
        &self.server_version
    }

    /// Set the per-operation timeout used by the iiod server (not the
    /// socket read/write timeout — that's on the client side). Useful
    /// when streaming RX, where a buffer pull at 576 kSa/s is expected
    /// to return inside a few ms; for control-plane attrs the server
    /// default is fine.
    pub fn set_iiod_timeout(&mut self, ms: u32) -> Result<(), IiodError> {
        self.transport.send_line(&format!("TIMEOUT {ms}"))?;
        let line = self.transport.recv_line()?;
        let status = parse_status(&line, "TIMEOUT")?;
        status_to_size(status, "TIMEOUT")?;
        Ok(())
    }

    /// Read a *device-level* attribute (i.e. one that lives on the
    /// device, not on a channel — `filter_fir_config` and
    /// `gain_control_mode_available` are typical examples).
    ///
    /// The returned string is trimmed of the trailing NUL byte
    /// libiio includes in its byte count, and of any trailing
    /// whitespace (some attributes end with a stray `\n`).
    pub fn read_dev_attr(&mut self, device: &str, attr: &str) -> Result<String, IiodError> {
        let cmd = format!("READ {device} {attr}");
        self.transport.send_line(&cmd)?;
        self.read_attr_response(&cmd)
    }

    /// Write a device-level attribute.
    pub fn write_dev_attr(
        &mut self,
        device: &str,
        attr: &str,
        value: &str,
    ) -> Result<(), IiodError> {
        // Server expects the value as a NUL-terminated string. Length
        // sent in the command line is the byte count we'll push next.
        let payload = nul_terminated(value);
        let ctx = format!("WRITE {device} {attr}");
        let cmd = format!("WRITE {device} {attr} {}", payload.len());
        self.transport.send_line(&cmd)?;
        self.transport.send_bytes(&payload)?;
        self.read_write_status(&ctx, payload.len())
    }

    /// Read a channel attribute.
    pub fn read_chn_attr(
        &mut self,
        device: &str,
        dir: ChanDir,
        chan: &str,
        attr: &str,
    ) -> Result<String, IiodError> {
        let cmd = format!("READ {device} {} {chan} {attr}", dir.as_iiod_token());
        self.transport.send_line(&cmd)?;
        self.read_attr_response(&cmd)
    }

    /// Write a channel attribute.
    pub fn write_chn_attr(
        &mut self,
        device: &str,
        dir: ChanDir,
        chan: &str,
        attr: &str,
        value: &str,
    ) -> Result<(), IiodError> {
        let payload = nul_terminated(value);
        let ctx = format!(
            "WRITE {device} {} {chan} {attr}",
            dir.as_iiod_token()
        );
        let cmd = format!(
            "WRITE {device} {} {chan} {attr} {}",
            dir.as_iiod_token(),
            payload.len()
        );
        self.transport.send_line(&cmd)?;
        self.transport.send_bytes(&payload)?;
        self.read_write_status(&ctx, payload.len())
    }

    /// Pull the IIO context XML via `PRINT`. Useful for debugging
    /// and for cross-checking that the device list (cf-ad9361-lpc,
    /// cf-ad9361-dds-core-lpc, ad9361-phy) is what we expect.
    ///
    /// On modern iiod, the response is **length-prefixed** with a
    /// `%li\n` line (matching the `READ`/`READBUF` shape). Older
    /// firmwares stream the XML directly until a blank line; we go
    /// with the modern shape since Pluto firmwares targeted by this
    /// project ship a recent libiio.
    pub fn print_xml(&mut self) -> Result<String, IiodError> {
        self.transport.send_line("PRINT")?;
        let status_line = self.transport.recv_line()?;
        let status = parse_status(&status_line, "PRINT")?;
        let n = status_to_size(status, "PRINT")?;
        let bytes = self.transport.recv_exact(n)?;
        self.transport.consume_trailing_newline()?;
        Ok(String::from_utf8(bytes)?)
    }

    /// Send `EXIT` to politely tear the connection down. The server
    /// hangs up; we drop the socket in `Drop`.
    pub fn close(mut self) -> Result<(), IiodError> {
        // EXIT is fire-and-forget on modern iiod (the parser accepts
        // the token but takes no action — the server closes when the
        // socket EOFs anyway). We send it for compatibility with
        // older builds and don't wait for a reply.
        let _ = self.transport.send_line("EXIT");
        Ok(())
    }

    // -----------------------------------------------------------------
    // Buffer-streaming commands
    // -----------------------------------------------------------------
    //
    // OPEN allocates a kernel-side IIO buffer and enables the channels
    // selected by `mask` on `device`. From that point on, READBUF
    // (RX-side devices) or WRITEBUF (TX-side) move samples through.
    // CLOSE tears the buffer down; the client connection survives and
    // can re-OPEN against the same or a different device.
    //
    // For Pluto:
    //   * RX device = `cf-ad9361-lpc`, channels voltage0 (real I) and
    //     voltage1 (real Q). Mask = 0x3 (both enabled).
    //   * TX device = `cf-ad9361-dds-core-lpc`, same channel layout,
    //     same mask. The DDS gets bypassed when the buffer is fed from
    //     userland.
    //   * Sample format is `le:S16/16>>0` (TX) and `le:S12/16>>0` (RX:
    //     12-bit value in the low bits of a sign-extended 16-bit slot).
    //     Either way the wire byte layout is identical: I_lo, I_hi,
    //     Q_lo, Q_hi, repeat. 4 bytes per scan cycle.

    /// Open a streaming buffer on `device`.
    ///
    /// `samples_per_buffer` is the IIO buffer size in *scan cycles*
    /// (i.e. one (I, Q) pair on Pluto). Pick a value that's large
    /// enough to hide kernel-side jitter but small enough to keep
    /// per-buffer latency manageable: 32768 = 64 kB at 4 B / sample,
    /// fills in ~57 ms at 576 kSa/s — a good default.
    ///
    /// `mask` is the channel-enable bitmask. Channels are encoded
    /// as 32-bit zero-padded hex on the wire (`%08x`); for Pluto
    /// you almost always want `0x3`.
    ///
    /// `cyclic = true` makes the kernel replay the buffer endlessly
    /// once filled — used by TX test tones. RX should always pass
    /// `cyclic = false`.
    pub fn open_buffer(
        &mut self,
        device: &str,
        samples_per_buffer: usize,
        mask: u32,
        cyclic: bool,
    ) -> Result<(), IiodError> {
        let cyclic_token = if cyclic { " CYCLIC" } else { "" };
        let cmd = format!(
            "OPEN {device} {samples_per_buffer} {mask:08x}{cyclic_token}"
        );
        self.transport.send_line(&cmd)?;
        let line = self.transport.recv_line()?;
        let status = parse_status(&line, &cmd)?;
        // Non-zero status from OPEN is always an error code (negative
        // -errno). Zero means success — there's no payload size to
        // interpret here, unlike READ.
        if status < 0 {
            return Err(IiodError::ServerErrno {
                errno: (-status) as i32,
                context: cmd,
            });
        }
        Ok(())
    }

    /// Close a previously-opened streaming buffer.
    pub fn close_buffer(&mut self, device: &str) -> Result<(), IiodError> {
        let cmd = format!("CLOSE {device}");
        self.transport.send_line(&cmd)?;
        let line = self.transport.recv_line()?;
        let status = parse_status(&line, &cmd)?;
        if status < 0 {
            return Err(IiodError::ServerErrno {
                errno: (-status) as i32,
                context: cmd,
            });
        }
        Ok(())
    }

    /// Pull samples until `dest` is full or the server signals EOF.
    ///
    /// iiod's READBUF response is **chunked**: the server may answer
    /// in several pieces if the requested byte count exceeds the
    /// kernel buffer size. Each chunk carries:
    ///
    /// ```text
    /// <chunk_bytes>\n        status line; positive = bytes follow,
    ///                        negative = -errno, zero = end of buffer
    /// <channel_mask_hex>\n   the mask we set at OPEN, echoed back
    /// <chunk_bytes>          binary scan-cycle data
    /// ```
    ///
    /// The mask line lets clients on multi-channel devices figure
    /// out which scan elements are interleaved in the payload —
    /// for Pluto we know the answer (mask = 0x3, 2 channels) so we
    /// just consume it. Returns the number of bytes actually written
    /// into `dest`. Short reads can happen on EOF; the caller
    /// decides whether that's an error.
    pub fn read_buffer_into(
        &mut self,
        device: &str,
        dest: &mut [u8],
    ) -> Result<usize, IiodError> {
        let cmd = format!("READBUF {device} {}", dest.len());
        self.transport.send_line(&cmd)?;

        let mut filled = 0usize;
        while filled < dest.len() {
            let status_line = self.transport.recv_line()?;
            let status = parse_status(&status_line, &cmd)?;
            if status < 0 {
                return Err(IiodError::ServerErrno {
                    errno: (-status) as i32,
                    context: format!("{cmd} (after {filled} B)"),
                });
            }
            let chunk_bytes = status as usize;
            if chunk_bytes == 0 {
                // Server signals end of stream: no more data
                // forthcoming for this READBUF call. Return what
                // we got so far — caller treats short read.
                break;
            }
            // Channel-mask echo line. Always present when status > 0
            // per the iiod protocol contract; we don't validate it
            // (OPEN already pinned the mask).
            let _mask_line = self.transport.recv_line()?;

            if filled + chunk_bytes > dest.len() {
                return Err(IiodError::Protocol(format!(
                    "{cmd}: server sent {chunk_bytes}-byte chunk but \
                     only {} bytes remain in dest",
                    dest.len() - filled
                )));
            }
            self.transport
                .recv_exact_into(&mut dest[filled..filled + chunk_bytes])?;
            filled += chunk_bytes;
        }
        Ok(filled)
    }

    /// Push `src.len()` bytes into the device's TX buffer. Returns the
    /// number of bytes the server accepted (may be less than asked
    /// if the kernel buffer fills before the client buffer drains —
    /// caller loops on the residue).
    pub fn write_buffer(&mut self, device: &str, src: &[u8]) -> Result<usize, IiodError> {
        let cmd = format!("WRITEBUF {device} {}", src.len());
        self.transport.send_line(&cmd)?;
        self.transport.send_bytes(src)?;
        let line = self.transport.recv_line()?;
        let status = parse_status(&line, &cmd)?;
        let n = status_to_size(status, &cmd)?;
        Ok(n)
    }

    // -----------------------------------------------------------------
    // private helpers
    // -----------------------------------------------------------------

    /// Common tail for both READ-flavoured commands: read the status
    /// line, if positive read that many bytes of payload + the
    /// trailing `\n`, decode as UTF-8, drop the NUL byte libiio
    /// includes in its count, trim trailing whitespace.
    fn read_attr_response(&mut self, ctx: &str) -> Result<String, IiodError> {
        let status_line = self.transport.recv_line()?;
        let status = parse_status(&status_line, ctx)?;
        let n = status_to_size(status, ctx)?;
        let bytes = self.transport.recv_exact(n)?;
        self.transport.consume_trailing_newline()?;
        let mut s = String::from_utf8(bytes)?;
        // libiio's iio_*_attr_read returns strlen+1 (incl. NUL) so
        // the wire byte count we requested includes that trailing
        // NUL. Strip it. Also strip any trailing whitespace some
        // attrs include (`\n` on `gain_control_mode_available`).
        if s.ends_with('\0') {
            s.pop();
        }
        let trimmed_len = s.trim_end().len();
        s.truncate(trimmed_len);
        Ok(s)
    }

    /// Common tail for both WRITE-flavoured commands: read the status
    /// line and check it equals the byte count we sent. iiod returns
    /// `-errno` on failure (e.g. -22 EINVAL on out-of-range value).
    fn read_write_status(&mut self, ctx: &str, expected: usize) -> Result<(), IiodError> {
        let status_line = self.transport.recv_line()?;
        let status = parse_status(&status_line, ctx)?;
        let n = status_to_size(status, ctx)?;
        if n != expected {
            return Err(IiodError::Protocol(format!(
                "{ctx}: server acknowledged {n} bytes but we sent {expected}"
            )));
        }
        Ok(())
    }
}

/// Append a NUL byte to `value` (libiio's WRITE wire format expects
/// strings as NUL-terminated regions). Returned vec is the exact
/// payload to push after the command line.
fn nul_terminated(value: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(value.len() + 1);
    v.extend_from_slice(value.as_bytes());
    v.push(0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pluto's tezuka firmware emits version lines like
    /// `0.25.tezuka-` — make sure we parse those cleanly.
    #[test]
    fn parses_pluto_version_line() {
        let v = ServerVersion::parse("0.25.tezuka-");
        assert_eq!(v.major, 0);
        assert_eq!(v.minor, 25);
        assert_eq!(v.git, "tezuka-");
    }

    /// A regular libiio install on a host PC.
    #[test]
    fn parses_libiio_release_version() {
        let v = ServerVersion::parse("0.26.a0eca0d");
        assert_eq!(v.major, 0);
        assert_eq!(v.minor, 26);
        assert_eq!(v.git, "a0eca0d");
    }

    /// Garbage in → no panic, fields land in `git` for the operator
    /// to read in logs.
    #[test]
    fn version_parse_does_not_panic_on_garbage() {
        let v = ServerVersion::parse("???");
        assert_eq!(v.major, 0);
        assert_eq!(v.minor, 0);
        assert_eq!(v.git, "???");
    }

    #[test]
    fn chan_dir_wire_tokens() {
        assert_eq!(ChanDir::Input.as_iiod_token(), "INPUT");
        assert_eq!(ChanDir::Output.as_iiod_token(), "OUTPUT");
    }

    #[test]
    fn nul_terminated_appends_zero() {
        let v = nul_terminated("145500000");
        assert_eq!(v, b"145500000\0");
    }

    #[test]
    fn nul_terminated_handles_empty() {
        let v = nul_terminated("");
        assert_eq!(v, b"\0");
    }
}

//! Pure-Rust iiod (libiio TCP daemon) client.
//!
//! This is the transport layer that lets us talk to a PlutoSDR — or
//! any libiio-managed device exposing iiod over TCP — without touching
//! the C `libiio` library. Pluto's firmware ships an iiod listener on
//! TCP port 30431, reachable via the USB-NCM virtual NIC (the AD USB
//! driver stands one up at 192.168.2.1 by default, but a real Ethernet
//! port works the same way) or any user-supplied IP / hostname.
//!
//! ## Why pure Rust
//!
//! `libiio-sys` 0.4 (the upstream FFI binding) is `#[cfg(unix)]`-only,
//! so the standard `industrial-io` crate cannot compile on Windows
//! even though `libiio.dll` itself ships an official Windows installer.
//! Rather than maintain a Windows fork of the FFI, we sidestep the
//! whole C library: iiod's wire protocol is documented and stable, and
//! Pluto over USB on Windows is *already* a TCP-over-USB-NCM channel
//! under the hood — going TCP at the application layer just removes a
//! redundant translation. One code path, zero native deps, identical
//! on Windows / Linux / Pi / macOS.
//!
//! ## Wire protocol summary
//!
//! Text command lines, `\n`-terminated. Status responses are decimal
//! `%li\n` — positive = byte count of payload that follows, negative
//! = `-errno`. Commands we use:
//!
//! | Command | Use |
//! |---|---|
//! | `VERSION\n` | Initial handshake; server replies `MAJOR.MINOR.GIT-7\n` |
//! | `PRINT\n` | Returns the IIO-context XML (length-prefixed) |
//! | `READ <dev> <attr>\n` | Read a device attribute |
//! | `READ <dev> INPUT\|OUTPUT <chan> <attr>\n` | Read a channel attribute |
//! | `WRITE <dev> <attr> <bytes>\n<bytes>` | Write a device attribute |
//! | `WRITE <dev> INPUT\|OUTPUT <chan> <attr> <bytes>\n<bytes>` | Channel attr |
//! | `OPEN <dev> <samples> <mask> [CYCLIC]\n` | Open a streaming buffer |
//! | `CLOSE <dev>\n` | Close a buffer |
//! | `READBUF <dev> <byte_count>\n` | Pull samples (chunked) |
//! | `WRITEBUF <dev> <byte_count>\n<bytes>` | Push samples |
//! | `TIMEOUT <ms>\n` | Set per-op blocking timeout |
//! | `EXIT\n` | Tear down the connection |
//!
//! Reference: <https://github.com/analogdevicesinc/libiio/blob/main/iiod/parser.y>
//! and `iiod/ops.c` in the same tree for the per-command response shape.
//!
//! ## Threading
//!
//! Each `IiodClient` owns one TCP connection and is `!Sync`. iiod
//! itself spawns one server thread per connection, so opening multiple
//! `IiodClient`s against the same Pluto is fine — and that's how we
//! plan to split control plane (attrs, FIR loading, freq tuning) from
//! the streaming path (RX and TX each on their own connection) once
//! the buffer commands land.

pub mod codec;
pub mod error;
pub mod target;

mod client;

pub use client::{ChanDir, IiodClient, ServerVersion};
pub use error::IiodError;
pub use target::{parse_pluto_target, IIOD_DEFAULT_PORT};

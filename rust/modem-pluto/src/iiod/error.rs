//! Errors returned by the iiod client.

use std::io;

use thiserror::Error;

/// Anything that can go wrong while talking iiod.
///
/// Two broad classes:
/// - **Local I/O / parsing** failures (`Io`, `Protocol`, `Utf8`,
///   `BadStatusLine`) — the connection itself is sick, we bail.
/// - **Server-reported errors** (`ServerErrno`) — the connection is
///   fine but the iiod server returned a negative status code (i.e.
///   `-errno`). We surface the raw value so callers can match on
///   `EINVAL` (-22), `ENODEV` (-19), etc., without depending on a
///   platform `errno` crate.
#[derive(Debug, Error)]
pub enum IiodError {
    /// Underlying socket / read / write failure.
    #[error("iiod I/O: {0}")]
    Io(#[from] io::Error),

    /// Failed to parse the supplied URI / `host:port` string.
    #[error("iiod target parse: {0}")]
    BadTarget(String),

    /// Failed to resolve the supplied host (DNS / mDNS).
    #[error("iiod resolve '{host}': {source}")]
    Resolve { host: String, source: io::Error },

    /// Server replied with a status line we couldn't decode as
    /// `%li\n` — connection is desynchronized, caller should drop it.
    #[error("iiod bad status line: {0:?}")]
    BadStatusLine(String),

    /// Server returned a negative status code, i.e. `-errno`. We
    /// keep the absolute value here so callers can compare with the
    /// usual constants (22 = EINVAL, 19 = ENODEV, …).
    #[error("iiod server error (errno {errno}) on {context}")]
    ServerErrno { errno: i32, context: String },

    /// Server response was structurally wrong — e.g. payload didn't
    /// match the byte count it announced, or the trailing newline
    /// was missing where the protocol mandates one.
    #[error("iiod protocol violation: {0}")]
    Protocol(String),

    /// Attribute payload wasn't valid UTF-8. iiod attribute values
    /// are always ASCII / UTF-8 strings in practice, so we surface
    /// this rather than silently lossy-decoding.
    #[error("iiod payload not utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

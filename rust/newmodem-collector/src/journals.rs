//! Modem journals: submission, public index (HTML and JSON) and download.
//!
//! Stored apart from the soundings:
//! `<journals_dir>/<YYYY-MM-DD>/<callsign>_<HHMMSS>_<hash>/` holding
//! `metadata.json` and `journal.jsonl.gz`. The metadata is written last:
//! a journal is listed once it parses, hence never half written.
//!
//! The gzip is never decompressed here: it is untrusted, and the HMAC
//! secret ships inside every client binary. Everything the index shows
//! comes from the typed metadata and is escaped.

use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use chrono::Utc;
use html_escape::{encode_double_quoted_attribute as attr, encode_text as text};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use tracing::info;

use crate::{
    callsign_segment, is_safe_segment, read_dir_sorted, signed_timestamp, valid_callsign,
    verify_signature, write_file, ApiError, AppState, SubmitResponse, INDEX_FOOTER, INDEX_HEADER,
};

/// Largest accepted gzip; the GUI bundles at most this much.
pub const MAX_JOURNAL_BYTES: usize = 20 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;
const FILE_NAME: &str = "journal.jsonl.gz";
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// What the index shows. Computed by the client, hence only indicative.
#[derive(Debug, Default, Deserialize, Serialize)]
struct JournalMeta {
    callsign: String,
    #[serde(default)]
    gui_version: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    duration_s: Option<f64>,
    #[serde(default)]
    queue_max_ms: Option<f64>,
    #[serde(default)]
    incidents: Option<u32>,
    /// The previous session did not shut down cleanly.
    #[serde(default)]
    crashed: bool,
    #[serde(default)]
    notes: Option<String>,
}

pub async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<SubmitResponse>, ApiError> {
    let (signature, client_ts) = signed_timestamp(&headers)?;

    let (mut callsign, mut metadata, mut journal) = (None, None, None);
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad(format!("multipart: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad(format!("read field: {e}")))?;
        match name.as_str() {
            "callsign" => {
                let s = std::str::from_utf8(&data)
                    .map_err(|_| ApiError::bad("callsign not utf-8"))?;
                callsign = Some(s.trim().to_uppercase());
            }
            "metadata" if data.len() <= MAX_METADATA_BYTES => metadata = Some(data),
            "metadata" => return Err(ApiError::bad("metadata too large")),
            "journal" if data.len() <= MAX_JOURNAL_BYTES => journal = Some(data),
            "journal" => return Err(ApiError::bad("journal too large")),
            _ => {}
        }
    }
    let callsign = callsign.ok_or_else(|| ApiError::bad("missing 'callsign' field"))?;
    let metadata = metadata.ok_or_else(|| ApiError::bad("missing 'metadata' field"))?;
    let journal = journal.ok_or_else(|| ApiError::bad("missing 'journal' field"))?;
    if !valid_callsign(&callsign) || callsign.len() > 16 {
        return Err(ApiError::bad("invalid callsign"));
    }

    // Unlike a sounding's WAV, the payload is inside the signature.
    let body_hash = {
        let mut h = Sha256::new();
        h.update(&metadata);
        h.update(&journal);
        h.finalize()
    };
    verify_signature(&callsign, client_ts, &body_hash, &signature)?;

    if !journal.starts_with(&GZIP_MAGIC) {
        return Err(ApiError::bad("journal is not gzip"));
    }
    let meta: JournalMeta = serde_json::from_slice(&metadata)
        .map_err(|e| ApiError::bad(format!("metadata.json parse: {e}")))?;
    if meta.callsign.trim().to_uppercase() != callsign {
        return Err(ApiError::bad("callsign field mismatches metadata.json"));
    }

    let incoming = (metadata.len() + journal.len()) as u64;
    if stored_bytes(&state).await? + incoming > state.journals_quota_bytes {
        return Err(ApiError::server("journal storage full, contact the operator"));
    }

    let now = Utc::now();
    let date = now.format("%Y-%m-%d").to_string();
    let folder = format!(
        "{}_{}_{}",
        callsign_segment(&callsign),
        now.format("%H%M%S"),
        hex::encode(&body_hash[..4]),
    );
    let dir = state.journals_dir.join(&date).join(&folder);
    fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError::server(format!("mkdir journal: {e}")))?;
    write_file(&dir, FILE_NAME, &journal).await?;
    write_file(&dir, "metadata.json", &metadata).await?;
    info!("stored journal callsign={callsign} folder={date}/{folder}");

    Ok(Json(SubmitResponse {
        ok: true,
        url: download_url(&date, &folder),
        folder: format!("{date}/{folder}"),
    }))
}

/// `HHMMSS` of a `<callsign>_<HHMMSS>_<hash>` folder; callsigns have no `_`.
fn received_time(folder: &str) -> &str {
    folder.split('_').nth(1).unwrap_or("")
}

/// Every stored journal as (date, folder), newest first.
async fn stored(state: &AppState) -> Result<Vec<(String, String)>, ApiError> {
    let mut out = Vec::new();
    for date in read_dir_sorted(&state.journals_dir).await? {
        if !date.is_dir {
            continue;
        }
        let dir = state.journals_dir.join(&date.name);
        for entry in read_dir_sorted(&dir).await? {
            if entry.is_dir {
                out.push((date.name.clone(), entry.name));
            }
        }
    }
    out.sort_by(|a, b| (&b.0, received_time(&b.1)).cmp(&(&a.0, received_time(&a.1))));
    Ok(out)
}

async fn stored_bytes(state: &AppState) -> Result<u64, ApiError> {
    let mut total = 0;
    for (date, folder) in stored(state).await? {
        let dir = state.journals_dir.join(date).join(folder);
        for name in ["metadata.json", FILE_NAME] {
            if let Ok(m) = fs::metadata(dir.join(name)).await {
                total += m.len();
            }
        }
    }
    Ok(total)
}

fn download_url(date: &str, folder: &str) -> String {
    format!("/journals/{date}/{folder}/{FILE_NAME}")
}

/// One journal as listed by `GET /api/v1/journals`.
#[derive(Debug, Serialize)]
struct Listed {
    date: String,
    folder: String,
    /// Size of the gzip, for the downloader to check.
    bytes: u64,
    url: String,
    #[serde(flatten)]
    meta: JournalMeta,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    journals: Vec<Listed>,
}

/// Every complete journal, newest first.
async fn complete(state: &AppState) -> Result<Vec<Listed>, ApiError> {
    let mut out = Vec::new();
    for (date, folder) in stored(state).await? {
        let dir = state.journals_dir.join(&date).join(&folder);
        let Ok(raw) = fs::read(dir.join("metadata.json")).await else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice(&raw) else {
            continue;
        };
        let Ok(file) = fs::metadata(dir.join(FILE_NAME)).await else {
            continue;
        };
        out.push(Listed {
            url: download_url(&date, &folder),
            bytes: file.len(),
            date,
            folder,
            meta,
        });
    }
    Ok(out)
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Listing>, ApiError> {
    Ok(Json(Listing {
        journals: complete(&state).await?,
    }))
}

pub async fn index(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let entries = complete(&state).await?;
    let mut html = String::from(INDEX_HEADER);
    html.push_str(&format!(
        "<p><a href=\"/\">&larr; channel soundings</a></p>\
         <h2>Modem journals</h2><p class=\"summary\">{} journal(s) collected. \
         Machine-readable list: <a href=\"/api/v1/journals\">/api/v1/journals</a>.</p>\
         <table><thead><tr><th>Date</th><th>Callsign</th><th>Version</th><th>Host</th>\
         <th>Duration</th><th>Max queue</th><th>Incidents</th><th>Notes</th><th>Journal</th>\
         </tr></thead><tbody>",
        entries.len()
    ));
    for entry in &entries {
        html.push_str(&render_row(entry));
    }
    html.push_str("</tbody></table>");
    html.push_str(INDEX_FOOTER);
    Ok(Html(html))
}

fn render_row(entry: &Listed) -> String {
    let meta = &entry.meta;
    let or_dash = |v: &Option<String>| text(v.as_deref().unwrap_or("-")).into_owned();
    format!(
        "<tr><td><code>{}</code></td><td><strong>{}</strong>{}</td><td>{}</td><td>{}</td>\
         <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"{}\">download</a> ({} KiB)</td></tr>",
        text(&entry.date),
        text(&meta.callsign),
        if meta.crashed { " (crash)" } else { "" },
        or_dash(&meta.gui_version),
        or_dash(&meta.host),
        meta.duration_s.map_or("-".into(), |s| format!("{:.0} min", s / 60.0)),
        meta.queue_max_ms.map_or("-".into(), |ms| format!("{ms:.0} ms")),
        meta.incidents.map_or("-".into(), |n| n.to_string()),
        or_dash(&meta.notes),
        attr(&entry.url),
        entry.bytes.div_ceil(1024),
    )
}

pub async fn download(
    State(state): State<AppState>,
    AxumPath((date, folder, file)): AxumPath<(String, String, String)>,
) -> Response {
    if !is_safe_segment(&date) || !is_safe_segment(&folder) || file != FILE_NAME {
        return ApiError::bad("invalid path").into_response();
    }
    let path = state.journals_dir.join(&date).join(&folder).join(FILE_NAME);
    let Ok(body) = fs::read(&path).await else {
        return (StatusCode::NOT_FOUND, "no such journal\n").into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "application/gzip"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"journal.jsonl.gz\"",
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_supplied_fields_are_escaped() {
        let meta = JournalMeta {
            callsign: "HB9TOB".into(),
            notes: Some("<script>alert(1)</script>".into()),
            host: Some("<img src=x>".into()),
            ..Default::default()
        };
        let row = render_row(&Listed {
            date: "2026-11-22".into(),
            folder: "x".into(),
            bytes: 1,
            url: download_url("2026-11-22", "X\" onmouseover=\"y"),
            meta,
        });
        assert!(!row.contains("<script>"));
        assert!(!row.contains("<img"));
        assert!(!row.contains("\" onmouseover=\""));
    }
}

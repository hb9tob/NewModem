//! HTTP client for the Phase-D collector.
//!
//! Builds the multipart payload of a sounding report from a raw capture
//! (WAV + event-log JSON) and POSTs it (HMAC-signed) to
//! `<base_url>/api/v1/sondage`.
//!
//! The HMAC secret is shared with the collector through two twin
//! `secret.txt` files (gitignored) - the "near-mandatory NewModem"
//! anti-abuse contract. Server details: see `rust/newmodem-collector/`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const HMAC_SECRET: &str = include_str!("../secret.txt");

/// Args passed from the frontend via the Tauri command `submit_capture`.
#[derive(Debug, Deserialize)]
pub struct SubmitCaptureArgs {
    pub wav_path: String,
    pub callsign: String,
    /// Base URL of the collector, no suffix (e.g. `https://hb9tob-modem.duckdns.org`).
    pub collector_url: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Event log serialized to JSON by the frontend. Stored as-is on the
    /// server as `report.json` (the server does not parse its contents).
    #[serde(default)]
    pub event_log_json: Option<String>,
}

/// Result returned to JS: relative URL of the report on the collector
/// (concatenate to `collector_url` to open it in a browser).
#[derive(serde::Serialize)]
pub struct SubmitResult {
    pub folder: String,
    pub url: String,
    pub bytes_uploaded: usize,
}

pub async fn submit(args: SubmitCaptureArgs) -> Result<SubmitResult, String> {
    let secret = HMAC_SECRET.trim();
    if secret.len() < 32 || secret.chars().all(|c| c == '0') {
        return Err(
            "secret HMAC non configuré (placeholder dans secret.txt). \
             Génère via `openssl rand -hex 32` côté GUI ET côté collector."
                .into(),
        );
    }
    let callsign = args.callsign.trim().to_uppercase();
    if callsign.is_empty() {
        return Err("indicatif vide (Paramètres → Indicatif)".into());
    }
    let base = args.collector_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("URL collecteur vide (Paramètres → Collecteur)".into());
    }
    let url = format!("{base}/api/v1/sondage");

    // Minimal metadata.json, matching `ReportMeta` on the server side.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let metadata = serde_json::json!({
        "callsign": callsign,
        "profile": args.profile,
        "notes": args.notes,
        "gui_version": env!("CARGO_PKG_VERSION"),
        "timestamp": ts,
        "source": "rx_capture",
    });
    let metadata_bytes =
        serde_json::to_vec(&metadata).map_err(|e| format!("metadata serialize: {e}"))?;

    // report.json = event log serialized as-is ("[]" if none provided).
    let report_bytes = args
        .event_log_json
        .as_deref()
        .unwrap_or("[]")
        .as_bytes()
        .to_vec();

    // Signed-body hash: metadata || report. Binaries (WAV) are NOT signed
    // here - their integrity relies on the TLS tunnel and, secondarily, on
    // a hash that can be added to report.json once Phase B starts emitting
    // more formal reports.
    let body_hash = {
        let mut h = Sha256::new();
        h.update(&metadata_bytes);
        h.update(&report_bytes);
        h.finalize()
    };
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| "HMAC init failed".to_string())?;
    mac.update(callsign.as_bytes());
    mac.update(b"|");
    mac.update(ts.to_string().as_bytes());
    mac.update(b"|");
    mac.update(&body_hash);
    let signature = hex::encode(mac.finalize().into_bytes());

    // Read the WAV. tokio::fs so we don't block the runtime on a large
    // file (captures can reach a few MB).
    let wav_bytes = tokio::fs::read(&args.wav_path)
        .await
        .map_err(|e| format!("lecture WAV {} : {}", args.wav_path, e))?;
    let wav_filename = std::path::Path::new(&args.wav_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("capture.wav")
        .to_string();
    let bytes_uploaded = metadata_bytes.len() + report_bytes.len() + wav_bytes.len();

    // Multipart. Field order is arbitrary on the server side (it indexes
    // by name), but we keep callsign first for readability in nginx logs
    // if anything goes wrong.
    let form = reqwest::multipart::Form::new()
        .text("callsign", callsign.clone())
        .part(
            "metadata",
            reqwest::multipart::Part::bytes(metadata_bytes)
                .file_name("metadata.json")
                .mime_str("application/json")
                .map_err(|e| e.to_string())?,
        )
        .part(
            "report",
            reqwest::multipart::Part::bytes(report_bytes)
                .file_name("report.json")
                .mime_str("application/json")
                .map_err(|e| e.to_string())?,
        )
        .part(
            "capture_wav",
            reqwest::multipart::Part::bytes(wav_bytes)
                .file_name(wav_filename)
                .mime_str("audio/wav")
                .map_err(|e| e.to_string())?,
        );

    // 2 minutes: covers multi-MB uploads over asymmetric ADSL.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    let resp = client
        .post(&url)
        .header("X-Newmodem-Signature", signature)
        .header("X-Newmodem-Timestamp", ts.to_string())
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {status} : {}", body.trim()));
    }
    // The server replies `{"ok":true,"folder":"...","url":"..."}`.
    #[derive(Deserialize)]
    struct ServerResponse {
        #[serde(default)]
        folder: String,
        #[serde(default)]
        url: String,
    }
    let parsed: ServerResponse =
        serde_json::from_str(&body).map_err(|e| format!("réponse serveur invalide: {e}"))?;
    Ok(SubmitResult {
        folder: parsed.folder,
        url: parsed.url,
        bytes_uploaded,
    })
}

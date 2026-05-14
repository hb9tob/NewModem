//! HTTP client for the Phase-D collector.
//!
//! Builds the multipart payload of a sounding report from a raw capture
//! (WAV + event-log JSON) and POSTs it (HMAC-signed) to
//! `<base_url>/api/v1/sondage`.
//!
//! The HMAC secret is shared with the collector through two twin
//! `secret.txt` files (gitignored) - the "near-mandatory NewModem"
//! anti-abuse contract. Server details: see `rust/newmodem-collector/`.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const HMAC_SECRET: &str = include_str!("../secret.txt");

/// Server-side reply shape (`{"ok":true,"folder":"...","url":"..."}`).
/// Shared by `submit` (RX captures) and `submit_sounding` (sounder runs).
#[derive(Deserialize)]
struct ServerResponse {
    #[serde(default)]
    folder: String,
    #[serde(default)]
    url: String,
}

/// Hex-encoded HMAC-SHA256 over `callsign | timestamp | sha256(metadata
/// || report)`. Server recomputes the same way (see
/// `newmodem-collector::verify_signature`).
fn compute_signature(
    secret: &str,
    callsign: &str,
    ts: i64,
    metadata_bytes: &[u8],
    report_bytes: &[u8],
) -> Result<String, String> {
    let body_hash = {
        let mut h = Sha256::new();
        h.update(metadata_bytes);
        h.update(report_bytes);
        h.finalize()
    };
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| "HMAC init failed".to_string())?;
    mac.update(callsign.as_bytes());
    mac.update(b"|");
    mac.update(ts.to_string().as_bytes());
    mac.update(b"|");
    mac.update(&body_hash);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Validate the HMAC secret is present and not a placeholder. Both
/// submit paths refuse to run with an unconfigured secret rather than
/// silently shipping an HMAC the server will reject anyway.
fn checked_secret() -> Result<&'static str, String> {
    let secret = HMAC_SECRET.trim();
    if secret.len() < 32 || secret.chars().all(|c| c == '0') {
        return Err(
            "secret HMAC non configuré (placeholder dans secret.txt). \
             Génère via `openssl rand -hex 32` côté GUI ET côté collector."
                .into(),
        );
    }
    Ok(secret)
}

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
    let secret = checked_secret()?;
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

    let signature =
        compute_signature(secret, &callsign, ts, &metadata_bytes, &report_bytes)?;

    // Read the WAV. tokio::fs so we don't block the runtime on a large
    // file (captures can reach a few MB).
    let wav_bytes = tokio::fs::read(&args.wav_path)
        .await
        .map_err(|e| format!("lecture WAV {} : {}", args.wav_path, e))?;
    let wav_filename = Path::new(&args.wav_path)
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

    post_signed_multipart(&url, &signature, ts, form, bytes_uploaded).await
}

/// Args passed from the frontend via the Tauri command `submit_sounding`.
///
/// The frontend already holds the `ChannelSignature` JSON it got back
/// from `sounding_analyze`, so we accept it inline rather than re-read
/// the on-disk file — the on-disk layout differs between TX-rendered
/// soundings (`<id>/signature.json`) and RX-standalone soundings
/// (`<capture-stem>.signature.json` next to the WAV).
///
/// Field names match the server's `ReportMeta` schema verbatim
/// (`rust/newmodem-collector/src/main.rs:132`) so the deserialiser on
/// the other end is happy without an extra translation step.
#[derive(Debug, Deserialize)]
pub struct SubmitSoundingArgs {
    /// Absolute path to the capture WAV (whichever it sits, including
    /// `~/Downloads/capture-<ts>.wav`). Only read when `include_wav`.
    pub wav_path: String,
    pub callsign: String,
    pub collector_url: String,
    /// The full ChannelSignature serialized as JSON — the same payload
    /// the frontend received from `sounding_analyze`. Stored verbatim
    /// as `report.json` on the server.
    pub signature_json: String,
    /// Local-station Maidenhead grid (`JN36ld`, …). Filled from
    /// `Settings.locator`.
    #[serde(default)]
    pub locator: Option<String>,
    /// Sounder profile (typically the family: `fm`, `qo100`, `ssb_hf`
    /// — or the sounder-rx mode dropdown selection).
    #[serde(default)]
    pub profile: Option<String>,
    /// Relay/repeater name if the run went over one (else
    /// `"none"`/null). Surfaces from `sounder-rx-relay`.
    #[serde(default)]
    pub relay: Option<String>,
    /// Far-end transmitter model. Surfaces from `sounder-rx-tx-model`.
    #[serde(default)]
    pub tx_model: Option<String>,
    /// Free-form notes. The frontend prepends the RX rig model here
    /// because the collector schema doesn't (yet) have a dedicated
    /// `rx_model` field.
    #[serde(default)]
    pub notes: Option<String>,
    /// When `true` the capture WAV (typically 5–30 MiB) is attached to
    /// the multipart so the server can re-run analysis. Capped at the
    /// collector's `--max-upload-mb`.
    #[serde(default)]
    pub include_wav: bool,
}

/// Build and POST a multipart submission for a sounding run. The
/// signature JSON the frontend just analysed becomes `report.json` on
/// the server side; a fresh metadata.json is generated here (callsign,
/// source=`sounding`, rig-chain notes, gui version, timestamp).
pub async fn submit_sounding(args: SubmitSoundingArgs) -> Result<SubmitResult, String> {
    let secret = checked_secret()?;
    let callsign = args.callsign.trim().to_uppercase();
    if callsign.is_empty() {
        return Err("indicatif vide (Paramètres → Indicatif)".into());
    }
    let base = args.collector_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("URL collecteur vide (Paramètres → Collecteur)".into());
    }
    let url = format!("{base}/api/v1/sondage");

    let report_bytes = args.signature_json.as_bytes().to_vec();
    if report_bytes.is_empty() {
        return Err("signature vide (re-lancer l'analyse)".into());
    }

    // Build a minimal metadata.json. The server's `submit_sondage`
    // requires the `callsign` field of the parsed JSON to match the
    // multipart `callsign` text — that's why we always re-stamp it
    // here even if the frontend already pre-filled it.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Match the server's ReportMeta schema fields one-for-one
    // (callsign, locator, profile, relay, tx_model, notes,
    // gui_version, timestamp). We keep a non-schema `source` marker so
    // the same endpoint can later branch RX captures vs soundings if
    // needed — extra fields are tolerated by serde, the parsed struct
    // simply ignores them.
    let locator = args.locator.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let profile = args.profile.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let relay = args.relay.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let tx_model = args.tx_model.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let notes = args.notes.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let metadata = serde_json::json!({
        "callsign": callsign,
        "locator": locator,
        "profile": profile,
        "relay": relay,
        "tx_model": tx_model,
        "notes": notes,
        "gui_version": env!("CARGO_PKG_VERSION"),
        "timestamp": ts,
        "source": "sounding",
    });
    let metadata_bytes = serde_json::to_vec(&metadata)
        .map_err(|e| format!("metadata serialize: {e}"))?;

    let signature_hmac =
        compute_signature(secret, &callsign, ts, &metadata_bytes, &report_bytes)?;

    let mut bytes_uploaded = metadata_bytes.len() + report_bytes.len();
    let mut form = reqwest::multipart::Form::new()
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
            // The server stores the body of this field as `report.json`
            // regardless of the file_name; keeping `signature.json`
            // makes the multipart self-describing in nginx logs.
            reqwest::multipart::Part::bytes(report_bytes)
                .file_name("signature.json")
                .mime_str("application/json")
                .map_err(|e| e.to_string())?,
        );

    if args.include_wav {
        let wav_path = Path::new(&args.wav_path);
        if args.wav_path.trim().is_empty() {
            return Err("WAV demandé mais chemin vide".into());
        }
        let wav_bytes = tokio::fs::read(wav_path)
            .await
            .map_err(|e| format!("lecture {} : {}", wav_path.display(), e))?;
        bytes_uploaded += wav_bytes.len();
        let wav_filename = wav_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("capture.wav")
            .to_string();
        form = form.part(
            "capture_wav",
            reqwest::multipart::Part::bytes(wav_bytes)
                .file_name(wav_filename)
                .mime_str("audio/wav")
                .map_err(|e| e.to_string())?,
        );
    }

    post_signed_multipart(&url, &signature_hmac, ts, form, bytes_uploaded).await
}

/// Shared POST path used by both submit flavours. Builds a reqwest
/// client with a 120 s timeout (covers multi-MB uploads on asymmetric
/// ADSL), attaches the HMAC signature + timestamp headers, and parses
/// the server's `{ok,folder,url}` reply.
async fn post_signed_multipart(
    url: &str,
    signature: &str,
    ts: i64,
    form: reqwest::multipart::Form,
    bytes_uploaded: usize,
) -> Result<SubmitResult, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    let resp = client
        .post(url)
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
    let parsed: ServerResponse =
        serde_json::from_str(&body).map_err(|e| format!("réponse serveur invalide: {e}"))?;
    Ok(SubmitResult {
        folder: parsed.folder,
        url: parsed.url,
        bytes_uploaded,
    })
}

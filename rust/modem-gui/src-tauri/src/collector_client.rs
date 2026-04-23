//! Client HTTP du collector Phase D.
//!
//! Construit le multipart d'un sondage à partir d'une capture brute (WAV
//! + event log JSON) et le POST signé HMAC à `<base_url>/api/v1/sondage`.
//!
//! Le secret HMAC est partagé avec le collector via deux fichiers
//! `secret.txt` jumeaux (gitignorés) — c'est le contrat anti-abus
//! "quasi obligatoire NewModem". Détails serveur : voir
//! `rust/newmodem-collector/`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const HMAC_SECRET: &str = include_str!("../secret.txt");

/// Args passés depuis le frontend via la commande Tauri `submit_capture`.
#[derive(Debug, Deserialize)]
pub struct SubmitCaptureArgs {
    pub wav_path: String,
    pub callsign: String,
    /// URL de base du collector, sans suffixe (ex: `https://hb9tob-modem.duckdns.org`).
    pub collector_url: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Event log sérialisé en JSON par le frontend. Stocké tel quel sur
    /// le serveur en `report.json` (le serveur ne parse pas son contenu).
    #[serde(default)]
    pub event_log_json: Option<String>,
}

/// Résultat retourné côté JS : URL relative vers le sondage sur le
/// collector (à concaténer à `collector_url` pour ouvrir dans un
/// navigateur).
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

    // metadata.json minimaliste, conforme à `ReportMeta` côté serveur.
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

    // report.json = event log sérialisé tel quel ("[]" si rien fourni).
    let report_bytes = args
        .event_log_json
        .as_deref()
        .unwrap_or("[]")
        .as_bytes()
        .to_vec();

    // Hash du body signé : metadata || report. Les binaires (WAV) ne sont
    // PAS signés ici — leur intégrité repose sur le tunnel TLS et
    // accessoirement sur un hash qui pourra être ajouté à report.json
    // quand Phase B générera des rapports plus formels.
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

    // Lecture du WAV. tokio::fs pour ne pas bloquer le runtime sur un gros
    // fichier (les captures peuvent atteindre quelques MB).
    let wav_bytes = tokio::fs::read(&args.wav_path)
        .await
        .map_err(|e| format!("lecture WAV {} : {}", args.wav_path, e))?;
    let wav_filename = std::path::Path::new(&args.wav_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("capture.wav")
        .to_string();
    let bytes_uploaded = metadata_bytes.len() + report_bytes.len() + wav_bytes.len();

    // Multipart. Ordre des fields = ordre arbitraire côté serveur (il
    // les indexe par nom), mais on garde callsign en premier pour la
    // lisibilité dans les logs nginx en cas de pépin.
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

    // 2 minutes : couvre les uploads multi-MB sur ADSL asymétrique.
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
    // Le serveur répond `{"ok":true,"folder":"...","url":"..."}`.
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

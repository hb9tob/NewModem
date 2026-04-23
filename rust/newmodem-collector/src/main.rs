//! Newmodem channel-sounding collector.
//!
//! Passe-plat HTTP minimaliste : reçoit des sondages POSTés par le GUI
//! NewModem, vérifie une signature HMAC, stocke chaque soumission dans un
//! dossier `<reports_dir>/<YYYY-MM-DD>/<callsign>_<HHMMSS>_<short_hash>/`,
//! et expose un index HTML qui glob ce répertoire.
//!
//! Aucune base de données : le filesystem EST la source de vérité. L'admin
//! peut `rm -rf` n'importe quel dossier, l'index reflète immédiatement.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use clap::Parser;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

type HmacSha256 = Hmac<Sha256>;

/// Secret HMAC partagé avec le GUI NewModem. Lu au build via `include_str!`
/// depuis `secret.txt` (gitignoré). Le `secret.txt.example` du repo
/// contient juste un placeholder pour que `cargo build` passe sans la vraie
/// valeur ; il faut copier ce fichier en `secret.txt` et y mettre 64 chars
/// hex (256 bits) générés par `openssl rand -hex 32`.
const HMAC_SECRET: &str = include_str!("../secret.txt");

/// Fenêtre de validité du timestamp dans la signature HMAC, en secondes.
/// Au-delà, on rejette pour éviter les replays. ±300 s couvre une dérive
/// raisonnable d'horloge client + voyage réseau.
const TIMESTAMP_WINDOW_SECS: i64 = 300;

#[derive(Parser, Debug)]
#[command(about = "Newmodem channel-sounding collector")]
struct Cli {
    /// Adresse d'écoute. En prod : 127.0.0.1:8080 derrière nginx.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Racine de stockage des rapports. Doit être writable par le user qui
    /// run le binaire.
    #[arg(long, default_value = "/var/lib/newmodem-collector/reports")]
    reports_dir: PathBuf,

    /// Taille max d'un upload, en mégaoctets. Garde-fou contre les abus.
    #[arg(long, default_value_t = 50)]
    max_upload_mb: usize,
}

#[derive(Clone)]
struct AppState {
    reports_dir: Arc<PathBuf>,
    max_upload_bytes: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "newmodem_collector=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let secret_trimmed = HMAC_SECRET.trim();
    if secret_trimmed.len() < 32 {
        anyhow::bail!(
            "secret.txt trop court ({} chars). Génère 64 chars hex via \
             `openssl rand -hex 32`.",
            secret_trimmed.len()
        );
    }
    fs::create_dir_all(&cli.reports_dir).await?;
    info!("storage dir = {}", cli.reports_dir.display());

    let state = AppState {
        reports_dir: Arc::new(cli.reports_dir.clone()),
        max_upload_bytes: cli.max_upload_mb * 1024 * 1024,
    };

    let app = Router::new()
        .route("/", get(index_html))
        .route("/sondage/:date/:folder", get(sondage_detail))
        .route("/api/v1/sondage", post(submit_sondage))
        .route("/api/v1/health", get(health))
        .nest_service(
            "/reports",
            ServeDir::new(cli.reports_dir.clone())
                .precompressed_gzip()
                .precompressed_br(),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    info!("listening on {}", cli.bind);
    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

async fn health() -> &'static str {
    "ok\n"
}

// ─────────────────────────────────────────── Submission

/// Métadonnées principales extraites de `metadata.json` ; le reste du JSON
/// est conservé tel quel sur disque mais le serveur n'en lit que ces
/// champs pour l'index. Champs optionnels = absents pour être tolérant aux
/// futures évolutions du format.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReportMeta {
    callsign: String,
    #[serde(default)]
    locator: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    relay: Option<String>,
    #[serde(default)]
    tx_model: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    gui_version: Option<String>,
    /// Timestamp UTC de l'enregistrement côté client (epoch sec).
    #[serde(default)]
    timestamp: Option<i64>,
}

#[derive(Serialize)]
struct SubmitResponse {
    ok: bool,
    folder: String,
    url: String,
}

async fn submit_sondage(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<axum::Json<SubmitResponse>, ApiError> {
    // 1. Headers HMAC.
    let sig_hex = headers
        .get("x-newmodem-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad("missing X-Newmodem-Signature"))?
        .to_string();
    let ts_str = headers
        .get("x-newmodem-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad("missing X-Newmodem-Timestamp"))?;
    let client_ts: i64 = ts_str
        .parse()
        .map_err(|_| ApiError::bad("X-Newmodem-Timestamp not an integer"))?;
    let now_ts = Utc::now().timestamp();
    if (now_ts - client_ts).abs() > TIMESTAMP_WINDOW_SECS {
        return Err(ApiError::bad("timestamp out of window (clock skew?)"));
    }

    // 2. Slurp tous les fields multipart en mémoire (cap = max_upload_bytes).
    //    Pour notre usage (un sondage = ~quelques MB), c'est OK et ça
    //    simplifie le calcul HMAC qui demande l'ordre concaténé.
    let mut total = 0usize;
    let mut callsign_field: Option<String> = None;
    let mut metadata_bytes: Option<Bytes> = None;
    let mut report_bytes: Option<Bytes> = None;
    let mut wav_bytes: Option<Bytes> = None;
    let mut png_bytes: Option<Bytes> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad(format!("multipart: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad(format!("read field {name}: {e}")))?;
        total = total.saturating_add(data.len());
        if total > state.max_upload_bytes {
            return Err(ApiError::bad(format!(
                "upload too large (> {} MiB)",
                state.max_upload_bytes / (1024 * 1024)
            )));
        }
        match name.as_str() {
            "callsign" => {
                callsign_field = Some(
                    String::from_utf8(data.to_vec())
                        .map_err(|_| ApiError::bad("callsign not utf-8"))?
                        .trim()
                        .to_uppercase(),
                );
            }
            "metadata" => metadata_bytes = Some(data),
            "report" => report_bytes = Some(data),
            "capture_wav" => wav_bytes = Some(data),
            "curves_png" => png_bytes = Some(data),
            other => warn!("ignoring unknown multipart field '{other}'"),
        }
    }

    let callsign = callsign_field.ok_or_else(|| ApiError::bad("missing 'callsign' field"))?;
    if callsign.is_empty() || !callsign.chars().all(|c| c.is_ascii_alphanumeric() || c == '/') {
        return Err(ApiError::bad("callsign must be ASCII alphanumeric (slash allowed)"));
    }
    let metadata_bytes = metadata_bytes.ok_or_else(|| ApiError::bad("missing 'metadata' field"))?;
    let report_bytes = report_bytes.ok_or_else(|| ApiError::bad("missing 'report' field"))?;

    // 3. HMAC : on signe `callsign | timestamp | sha256(metadata || report)`.
    //    Les pièces jointes binaires (wav/png) ne sont pas signées : leur
    //    intégrité est garantie par hash inclus dans report.json côté GUI.
    let body_hash = {
        let mut h = Sha256::new();
        h.update(&metadata_bytes);
        h.update(&report_bytes);
        h.finalize()
    };
    let mut mac = HmacSha256::new_from_slice(HMAC_SECRET.trim().as_bytes())
        .map_err(|_| ApiError::server("HMAC key init failed"))?;
    mac.update(callsign.as_bytes());
    mac.update(b"|");
    mac.update(client_ts.to_string().as_bytes());
    mac.update(b"|");
    mac.update(&body_hash);
    let sig_expected = hex::encode(mac.finalize().into_bytes());
    if !constant_time_eq(sig_expected.as_bytes(), sig_hex.trim().as_bytes()) {
        return Err(ApiError::unauthorized("bad signature"));
    }

    // 4. Parse metadata.json (validation minimale + extraction callsign
    //    pour cohérence avec le field).
    let meta: ReportMeta = serde_json::from_slice(&metadata_bytes)
        .map_err(|e| ApiError::bad(format!("metadata.json parse: {e}")))?;
    if meta.callsign.trim().to_uppercase() != callsign {
        return Err(ApiError::bad("callsign field mismatches metadata.json"));
    }

    // 5. Allocation du dossier `<date>/<callsign>_<HHMMSS>_<shorthash>/`.
    //    Date = jour côté serveur (UTC). HHMMSS = heure UTC du serveur.
    //    Short hash = 4 premiers octets du body_hash, pour disambiguer
    //    deux soumissions simultanées du même callsign.
    let now: DateTime<Utc> = Utc::now();
    let date = now.format("%Y-%m-%d").to_string();
    let hhmmss = now.format("%H%M%S").to_string();
    let short = hex::encode(&body_hash[..4]);
    let folder_name = format!("{callsign}_{hhmmss}_{short}");
    let folder_rel = format!("{date}/{folder_name}");
    let folder_abs = state.reports_dir.join(&date).join(&folder_name);
    fs::create_dir_all(&folder_abs)
        .await
        .map_err(|e| ApiError::server(format!("mkdir {}: {e}", folder_abs.display())))?;

    // 6. Écriture des fichiers.
    write_file(&folder_abs, "metadata.json", &metadata_bytes).await?;
    write_file(&folder_abs, "report.json", &report_bytes).await?;
    if let Some(b) = wav_bytes {
        write_file(&folder_abs, "capture.wav", &b).await?;
    }
    if let Some(b) = png_bytes {
        write_file(&folder_abs, "curves.png", &b).await?;
    }

    info!(
        "stored sondage callsign={} folder={}/{}",
        callsign, date, folder_name
    );

    Ok(axum::Json(SubmitResponse {
        ok: true,
        folder: folder_rel.clone(),
        url: format!("/sondage/{date}/{folder_name}"),
    }))
}

async fn write_file(dir: &Path, name: &str, body: &[u8]) -> Result<(), ApiError> {
    let path = dir.join(name);
    fs::write(&path, body)
        .await
        .map_err(|e| ApiError::server(format!("write {}: {e}", path.display())))
}

// ─────────────────────────────────────────── Index HTML

async fn index_html(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let mut entries = Vec::new();
    let mut date_dirs = read_dir_sorted(&state.reports_dir).await?;
    date_dirs.reverse(); // dates récentes en premier
    for date_entry in date_dirs {
        if !date_entry.is_dir {
            continue;
        }
        let date_path = state.reports_dir.join(&date_entry.name);
        let mut sondage_dirs = read_dir_sorted(&date_path).await?;
        sondage_dirs.reverse(); // sondages récents du jour en premier
        for sondage in sondage_dirs {
            if !sondage.is_dir {
                continue;
            }
            let meta_path = date_path.join(&sondage.name).join("metadata.json");
            let meta: Option<ReportMeta> = match fs::read(&meta_path).await {
                Ok(b) => serde_json::from_slice(&b).ok(),
                Err(_) => None,
            };
            entries.push(IndexRow {
                date: date_entry.name.clone(),
                folder: sondage.name.clone(),
                meta,
            });
        }
    }
    Ok(Html(render_index(&entries)))
}

struct IndexRow {
    date: String,
    folder: String,
    meta: Option<ReportMeta>,
}

fn render_index(rows: &[IndexRow]) -> String {
    use html_escape::encode_text as h;
    let mut html = String::new();
    html.push_str(INDEX_HEADER);
    html.push_str(&format!(
        "<p class=\"summary\">{} sondage(s) collecté(s).</p>",
        rows.len()
    ));
    if rows.is_empty() {
        html.push_str("<p class=\"empty\">Aucun sondage pour l'instant.</p>");
    } else {
        html.push_str("<table><thead><tr>\
            <th>Date</th><th>Indicatif</th><th>Profil</th><th>Locator</th>\
            <th>TX</th><th>Relais</th><th>Dossier</th></tr></thead><tbody>");
        for row in rows {
            let meta_default = ReportMeta {
                callsign: "?".to_string(),
                locator: None,
                profile: None,
                relay: None,
                tx_model: None,
                notes: None,
                gui_version: None,
                timestamp: None,
            };
            let m = row.meta.as_ref().unwrap_or(&meta_default);
            let url = format!("/sondage/{}/{}", row.date, row.folder);
            html.push_str(&format!(
                "<tr>\
                    <td><code>{}</code></td>\
                    <td><strong>{}</strong></td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td>{}</td>\
                    <td><a href=\"{}\">{}</a></td>\
                </tr>",
                h(&row.date),
                h(&m.callsign),
                h(m.profile.as_deref().unwrap_or("—")),
                h(m.locator.as_deref().unwrap_or("—")),
                h(m.tx_model.as_deref().unwrap_or("—")),
                h(m.relay.as_deref().unwrap_or("—")),
                h(&url),
                h(&row.folder),
            ));
        }
        html.push_str("</tbody></table>");
    }
    html.push_str(INDEX_FOOTER);
    html
}

const INDEX_HEADER: &str = r##"<!DOCTYPE html>
<html lang="fr"><head><meta charset="utf-8">
<title>NewModem — sondages canal collectés</title>
<style>
body{font-family:system-ui,sans-serif;background:#fafafa;color:#222;margin:0;padding:2rem 1.5rem;max-width:1200px;margin:auto;}
h1{color:#2b7fbf;border-bottom:3px solid #2b7fbf;padding-bottom:.4rem;}
.summary{color:#666;}
table{width:100%;border-collapse:collapse;margin-top:1rem;font-size:.92rem;}
th,td{border:1px solid #ddd;padding:.45rem .6rem;text-align:left;}
th{background:#eef6ff;color:#2b7fbf;}
tr:hover{background:#f5f9fc;}
code{background:#f3f3f3;padding:.1em .3em;border-radius:3px;font-family:Consolas,monospace;}
a{color:#2b7fbf;text-decoration:none;}
a:hover{text-decoration:underline;}
.empty{color:#999;font-style:italic;}
footer{margin-top:3rem;color:#999;font-size:.85rem;}
</style></head><body>
<h1>NewModem — sondages canal collectés</h1>
"##;

const INDEX_FOOTER: &str = r##"<footer>
Stockage filesystem brut : <code>/var/lib/newmodem-collector/reports/</code>.
Bin : <a href="/api/v1/health">/api/v1/health</a>.
</footer></body></html>
"##;

// ─────────────────────────────────────────── Sondage detail

async fn sondage_detail(
    State(state): State<AppState>,
    AxumPath((date, folder)): AxumPath<(String, String)>,
) -> Response {
    if !is_safe_segment(&date) || !is_safe_segment(&folder) {
        return ApiError::bad("invalid path").into_response();
    }
    let dir = state.reports_dir.join(&date).join(&folder);
    let meta_path = dir.join("metadata.json");
    let meta_bytes = match fs::read(&meta_path).await {
        Ok(b) => b,
        Err(_) => return Redirect::to("/").into_response(),
    };
    let meta: ReportMeta = match serde_json::from_slice(&meta_bytes) {
        Ok(m) => m,
        Err(_) => {
            return ApiError::server("metadata.json corrupted").into_response();
        }
    };
    let mut files = Vec::new();
    if let Ok(mut rd) = fs::read_dir(&dir).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().to_string();
            files.push(name);
        }
    }
    files.sort();

    use html_escape::encode_text as h;
    let mut html = String::new();
    html.push_str(INDEX_HEADER);
    html.push_str(&format!(
        "<p><a href=\"/\">← retour à l'index</a></p>\
         <h2>{} — {}</h2>\
         <table><tr><th>Champ</th><th>Valeur</th></tr>\
         <tr><td>Indicatif</td><td><strong>{}</strong></td></tr>\
         <tr><td>Profil</td><td>{}</td></tr>\
         <tr><td>Locator</td><td>{}</td></tr>\
         <tr><td>TX</td><td>{}</td></tr>\
         <tr><td>Relais</td><td>{}</td></tr>\
         <tr><td>Notes</td><td>{}</td></tr>\
         <tr><td>GUI version</td><td>{}</td></tr>\
         </table>",
        h(&date),
        h(&folder),
        h(&meta.callsign),
        h(meta.profile.as_deref().unwrap_or("—")),
        h(meta.locator.as_deref().unwrap_or("—")),
        h(meta.tx_model.as_deref().unwrap_or("—")),
        h(meta.relay.as_deref().unwrap_or("—")),
        h(meta.notes.as_deref().unwrap_or("—")),
        h(meta.gui_version.as_deref().unwrap_or("—")),
    ));
    html.push_str("<h3>Fichiers</h3><ul>");
    for f in &files {
        let url = format!("/reports/{date}/{folder}/{f}");
        html.push_str(&format!(
            "<li><a href=\"{}\">{}</a></li>",
            h(&url),
            h(f)
        ));
    }
    html.push_str("</ul>");
    html.push_str(INDEX_FOOTER);
    Html(html).into_response()
}

fn is_safe_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("..")
        && !s.contains('/')
        && !s.contains('\\')
        && s.chars().all(|c| c.is_ascii_graphic())
}

// ─────────────────────────────────────────── Helpers

struct DirEntryInfo {
    name: String,
    is_dir: bool,
}

async fn read_dir_sorted(dir: &Path) -> Result<Vec<DirEntryInfo>, ApiError> {
    let mut out = Vec::new();
    let mut rd = match fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().to_string();
        let ft = ent
            .file_type()
            .await
            .map_err(|e| ApiError::server(format!("file_type: {e}")))?;
        out.push(DirEntryInfo {
            name,
            is_dir: ft.is_dir(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─────────────────────────────────────────── Errors

struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            msg: msg.into(),
        }
    }
    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            msg: msg.into(),
        }
    }
    fn server(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        warn!(status = self.status.as_u16(), "{}", self.msg);
        (self.status, format!("{}\n", self.msg)).into_response()
    }
}

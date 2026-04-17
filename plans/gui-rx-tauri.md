# Plan — GUI RX modem NBFM (Tauri + HTML/JS vanilla)

## Contexte

Le modem Rust + framing v2 est maintenant fiable OTA (jusqu'à 100 kB à
100 % de convergence, compensation drift sound card intégrée). Prochaine
étape : GUI de réception qui remplace le CLI pour les utilisateurs
finaux, avec capture audio continue et sauvegarde automatique des
fichiers reçus.

Le TX reste en CLI pour l'instant (GUI TX à ajouter dans une phase
ultérieure). L'extension du payload avec métadonnées (nom de fichier +
indicatif) est intégrée dès la phase 1 pour que la GUI RX les affiche.

## Décisions architecturales (validées)

| Point | Choix |
|---|---|
| Framework app | **Tauri 2** (Rust backend + web UI, native access au sound card et FS) |
| Frontend | **Vanilla HTML + CSS + JS** (pas de build chain, Tauri suffit) |
| Audio capture | **`cpal`** (Rust, cross-platform, de facto standard) |
| Spectre | Canvas 2D + FFT Rust (crate `realfft` ou `rustfft`) |
| Métadonnées payload | **Préfixe binaire dans le payload utilisateur** (option 3B) : `[ver u8][len_fn u8][filename bytes][len_qrz u8][qrz bytes][content]`. N'impacte pas AppHeader. |
| Répertoire de sauvegarde | Configurable via bouton dans la top bar, persistant entre sessions (fichier config app) |
| Gestion multi-fichiers | Dernier/plus gros fichier affiché en grand. Barre de vignettes scrollable en bas avec historique (cap à ~30 entrées) |
| Écrase sur doublon | Oui, même nom → overwrite |
| **Auto-détection profil** | **Obligatoire en MVP** (objectif : zéro réglage pour l'utilisateur âgé/non-tech). RX probe tous les profils jusqu'à trouver un préambule + header valides, lock sur le profil gagnant pour la session. |

## Architecture globale

```
┌─────────────────────────────────────────────────────┐
│  Tauri app (nbfm-modem-gui)                         │
│                                                     │
│  ┌──────────────────┐     ┌──────────────────────┐  │
│  │ Frontend         │ ←→  │ Rust backend         │  │
│  │ (HTML/CSS/JS)    │ IPC │                      │  │
│  │                  │     │  • audio capture     │  │
│  │ • top bar        │     │  • rx_stream         │  │
│  │ • main 75%       │     │  • disk save         │  │
│  │ • spectre 25%    │     │                      │  │
│  │ • panel info     │     │  uses :              │  │
│  │                  │     │   modem-core         │  │
│  │                  │     │   cpal               │  │
│  └──────────────────┘     │   realfft            │  │
│                           │   tokio              │  │
│                           └──────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

## Décomposition en phases

### Phase 1 : Payload metadata envelope (modem-core)

Nouveau module `rust/modem-core/src/payload_envelope.rs` :

```rust
pub struct PayloadEnvelope {
    pub version: u8,         // = 1
    pub filename: String,    // ≤ 64 bytes UTF-8
    pub callsign: String,    // ≤ 10 bytes ASCII
    pub content: Vec<u8>,
}

impl PayloadEnvelope {
    pub fn encode(&self) -> Vec<u8>;  // → [1, len_fn, fn..., len_qrz, qrz..., content...]
    pub fn decode(buf: &[u8]) -> Option<Self>;
}
```

Frame layout user → wire :
```
user file content
      ↓
[PayloadEnvelope v1]
      ↓
build_superframe_v2(envelope.encode(), config, session_id, ...)
```

Avantage option 3B : aucun changement de AppHeader, version byte
permet évolutions futures sans casser de compat.

**Tests** :
- Round-trip encode/decode (unitaire)
- Integration avec build_superframe_v2 + rx_v2 sur un payload wrappé
- Fallback : si décode échoue (ancien format), retourner le buffer brut

### Phase 2 : CLI TX extension (modem-cli)

Ajouter au sous-commande `tx` :
- `--filename <str>` : défaut = nom du fichier d'entrée
- `--callsign <str>` : obligatoire en v2 (erreur clair sinon)

TX applique `PayloadEnvelope::encode` avant `build_superframe_v2`.

Exemple d'appel :
```bash
nbfm-modem tx --input image.avif --output out.wav \
  --profile NORMAL --frame-version 2 \
  --filename image.avif --callsign HB9TOB \
  --session-id DEADBEEF --vox 0.8
```

### Phase 3 : Streaming RX (modem-core → nouveau module rx_stream.rs)

Le `rx_v2` actuel consomme un buffer complet. Pour la GUI on a besoin
d'un décodeur incrémental qui traite des chunks d'audio au fil de
l'eau et émet des événements.

**Auto-détection profil** (objectif "zero réglage") : quand la
corrélation préambule dépasse un seuil dans le ring buffer, le
détecteur teste **les 5 profils** et sélectionne celui dont la
corrélation est la plus forte (le préambule est le même mais les
paramètres sps/pitch/β diffèrent — le profil correct donnera un pic
clair là où les autres sont bruit). Ensuite décodage v2 normal. Lock
du profil jusqu'à SessionEnd, puis re-probe pour la session suivante.

```rust
pub struct StreamReceiver {
    config: ModemConfig,
    state: StreamState,
    ring_buffer: VecDeque<f32>,
    // ... buffers MF, fse_input, tracking state
}

pub enum StreamEvent {
    PreambleDetected,
    HeaderDecoded(Header),
    AppHeaderDecoded(AppHeader),
    PayloadEnvelopeDecoded { filename: String, callsign: String, content_len: usize },
    ProgressUpdate { blocks_ok: usize, blocks_total: usize, sigma2: f64 },
    FileComplete { filename: String, callsign: String, content: Vec<u8> },
    SessionEnd,
    Error(String),
}

impl StreamReceiver {
    pub fn feed(&mut self, samples: &[f32]) -> Vec<StreamEvent>;
}
```

Stratégie simplifiée pour MVP : on buffer tout, on scan périodiquement
pour préambule, dès qu'on détecte → on lance le décodage v2 sur le
buffer (avec compensation drift). Quand fini → on reset et on scan à
nouveau pour la prochaine session.

Pour un vrai streaming fin-grain on ferait du par-chunk, mais le MVP
batch-per-session est largement suffisant et beaucoup plus simple.

**Tests** :
- Concatène plusieurs tx_v2 avec du silence entre → vérifier que
  chaque fichier est décodé séparément avec les bons événements.

### Phase 4 : Tauri app MVP (nouveau crate modem-gui)

Structure :
```
rust/modem-gui/
├── Cargo.toml
├── src-tauri/
│   ├── src/main.rs            # commandes Tauri + backend
│   ├── src/audio.rs           # cpal capture
│   ├── src/rx_worker.rs       # thread de décodage
│   ├── src/spectrum.rs        # FFT realtime
│   └── src/config.rs          # persistence settings
├── src/                       # frontend
│   ├── index.html
│   ├── style.css
│   ├── main.js
│   └── spectrum.js
└── tauri.conf.json
```

**Commandes backend exposées au frontend** :
- `list_audio_devices() -> Vec<DeviceInfo>` (id, name, sample_rate)
- `start_capture(device_id) -> Result<(), String>`
- `stop_capture()`
- `set_save_dir(path: PathBuf)`
- `get_save_dir() -> PathBuf`
- `set_expected_profile(name: Option<String>)` (None = auto-detect, not yet implemented; MVP default "NORMAL")
- `get_history() -> Vec<HistoryEntry>`

**Événements émis vers le frontend** (`app.emit("<event>", payload)`) :
- `spectrum_frame` : [f32; 256] log-magnitude FFT
- `preamble` : marker de début de session
- `header` : { profile, mode, payload_len }
- `app_header` : { session_id, file_size, K, T, hash }
- `file_meta` : { filename, callsign, size }
- `progress` : { blocks_ok, blocks_total, sigma2 }
- `file_complete` : { filename, callsign, saved_path, mime }
- `session_end` : reset UI

**UI layout** (index.html + style.css) :
```
┌──────────────────────────────────────────────────────┐
│  🎙 [Sound Card ▼] 💾 [Save dir: /path…] [⚙ Options] │  ← .top-bar (sticky)
├──────────────────────────────────────────┬───────────┤
│                                          │ SPECTRE   │
│  ┌─────────────────────────────────────┐ │ (canvas)  │
│  │   Fichier courant (ou dernier)      │ │           │
│  │                                     │ │           │
│  │   [img si image] ou infos           │ ├───────────┤
│  │                                     │ │ Mode      │
│  │   From: HB9TOB                      │ │ [HIGH]    │
│  │   Name: photo.avif                  │ │ r3/4      │
│  │   Size: 27 384 B                    │ │ 1500 Bd   │
│  │   Status: decoding 134/134 ✓        │ │           │
│  └─────────────────────────────────────┘ │ Progress  │
│                                          │ [██████]  │
│  ──── Historique ────                    │ 134/134   │
│  [📷][📄][📷][📷][📄]…  (scroll →)       │ σ²=0.014  │
│                                          │ 0xDEADBEEF│
└──────────────────────────────────────────┴───────────┘
```

**Comportement** :
- Vignettes cliquables → affichent leur fichier dans la zone principale
- Dernier fichier reçu auto-affiché comme "courant"
- Image rendue inline si MIME image/*
- Fichier non-image → infos + lien "Open folder"

### Phase 5 : Polish

- **Config persistante** (JSON dans `~/.config/nbfm-modem-gui/settings.json`) :
  save_dir, dernière device audio utilisée, max history count
- **Erreurs** visibles (toast ou panel) : pas de device, FS permission denied, session corrompue
- **Reprise session au démarrage** : charger l'historique depuis le dossier de sauvegarde (lire les fichiers récents + métadonnées associées)
- **Auto-détection profil** : essayer tous les profils jusqu'à ce qu'un préambule matche (futur — le MVP utilise un profil fixe sélectionné dans la top bar)

## Fichiers créés / modifiés

**Nouveaux** :
- `rust/modem-core/src/payload_envelope.rs` (phase 1)
- `rust/modem-core/src/rx_stream.rs` (phase 3)
- `rust/modem-gui/` (arborescence complète) (phase 4)

**Modifiés** :
- `rust/modem-core/src/lib.rs` : exporter nouveaux modules
- `rust/modem-cli/src/main.rs` : `--filename` et `--callsign` (phase 2)

**Intacts** : la chaîne DSP du modem (`ffe.rs`, `marker.rs`, `rx_v2.rs`, `frame.rs`, etc.) reste inchangée.

## Dépendances Cargo à ajouter

Au workspace :
- `tauri = "2"` (+ plugin `tauri-plugin-fs`, `tauri-plugin-dialog`)
- `cpal = "0.15"` (audio capture cross-platform)
- `realfft = "3"` (FFT rapide pour spectre)
- `serde = { "1", features = ["derive"] }` (sérialisation IPC)
- `tokio = { "1", features = ["rt", "sync"] }` (async backend)

## Critères d'acceptation phase par phase

| Phase | Critère |
|---|---|
| 1 | Round-trip envelope passe les tests ; build_superframe_v2 + rx_v2 décode correctement un payload wrappé avec filename + callsign |
| 2 | `nbfm-modem tx --filename X --callsign HB9XYZ …` produit un WAV qui, décodé, expose filename et callsign via `rx_v2` |
| 3 | Deux fichiers concaténés dans un flux sont décodés séparément via StreamReceiver, avec événements SessionEnd entre les deux |
| 4 | GUI démarre, affiche liste des sound cards, capture audio, affiche spectre live ; réception d'un WAV joué → affichage progress + fichier sauvegardé + affichage image si AVIF/JPG/PNG |
| 5 | GUI redémarrée retrouve son historique, son save_dir ; gère gracieusement l'erreur "device busy" |

## Hors scope de ce plan

- GUI TX (phase ultérieure)
- Auto-détection de profil (aide : l'utilisateur sélectionne le profil attendu dans la top bar pour le MVP ; auto-detect en polish)
- Sélection du mode audio (IPv6/Bluetooth/etc.) au-delà de ce que `cpal` expose par défaut
- RaptorQ / multi-round merge (phase 2.5 séparée)
- Packaging signé / distribution (AppImage, DMG, MSI) — ajouter quand le MVP marche

## Ordre d'implémentation suggéré

1. Phase 1 (payload envelope) → 200 lignes, 2 h
2. Phase 2 (CLI TX) → 50 lignes, 30 min
3. Phase 3 (stream RX) → 300 lignes, 4 h, test mélangé avec phase 4
4. Phase 4a (Tauri setup + sound card list) → 2 h
5. Phase 4b (capture + decode + events) → 4 h
6. Phase 4c (UI layout + history) → 3 h
7. Phase 5 (polish) → selon retours

Total estimé : ~15 h de dev effectif hors debug/polish.

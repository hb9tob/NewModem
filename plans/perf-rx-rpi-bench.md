# Bench RX idle sur Raspberry Pi 4 et Raspberry Pi 5

**Date d'écriture** : 2026-04-26
**À exécuter par** : agent Claude Code installé sur un Raspberry Pi (ARM Linux),
même repo git que le projet principal.
**Contexte mémoire utile** : `MEMORY.md`, en particulier
`feedback_no_touch_working.md`, `feedback_telecom_rigor.md`,
`project_v3_refactor_done.md`. Et le plan précédent
`plans/perf-rx-idle-surface-pro-7.md` (méthodologie identique, autre HW).

---

## Contexte

Modem audio Rust pour radio amateur NBFM. Le RX en idle (pas de signal
valide) doit pouvoir tenir le tick `SCAN_INTERVAL_MS` de 1000 ms.

Sur **Surface Pro 7** (Intel Core i5-1035G4, AVX2), on a mesuré le
2026-04-26 (cf. `results/perf-idle-sp7-*.txt`) :

| Tick idle | SP7 mesuré |
|---|---|
| Legacy (avant gate)        | ~1003 ms |
| Avec gate FFT préambule    | ~1.6 ms |
| Pic décodage Ultra (signal présent) | ~22 ms |

**But du bench Pi** : valider sur les deux cibles ARM réalistes
(Pi 4 / Pi 5) que le tick idle reste largement sous le budget 1 s, et
mesurer la marge réelle vs estimation théorique.

Estimation théorique (rapport clock × IPC × NEON vs AVX2) :

| Tick idle | RPi 5 (Cortex-A76 @ 2.4 GHz) | RPi 4 (Cortex-A72 @ 1.5 GHz) |
|---|---|---|
| Legacy                     | ~1500 ms     | ~5000–6000 ms |
| Avec gate (actuel)         | ~2.5 ms      | ~9–10 ms |
| Pic Ultra                  | ~35 ms       | ~120 ms |

À confirmer ou infirmer.

---

## Plan d'exécution

### 1. Identifier la machine

```bash
cat /proc/cpuinfo | grep -E '^(Model|Hardware|model name|Revision)' | head -10
cat /proc/meminfo | head -3
uname -m
lsb_release -a 2>/dev/null
```

Récupérer :
- Modèle exact : `Raspberry Pi 4 Model B Rev 1.5` ou `Raspberry Pi 5 Model B Rev 1.0`
- RAM totale (le build cargo a besoin d'au moins 2 Go libres pour
  compiler les deps Tauri-related ; le bench seul tient en 200 Mo)
- Architecture : `aarch64` attendu (64-bit Pi OS). Si `armv7l` (32-bit),
  **arrêter** et signaler — la suite assume ARM64.

Étiqueter la cible : `rpi4` ou `rpi5`. Cette étiquette servira pour
nommer le fichier de résultat.

### 2. Pré-requis système

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libasound2-dev git curl
```

`libasound2-dev` n'est pas strictement nécessaire pour
`-p modem-core` (workspace minimal), mais l'inclure évite un échec
silencieux si `cargo build --workspace` est lancé par erreur plus tard.

### 3. Toolchain Rust

Le Rust apt de Debian/Raspberry Pi OS est trop vieux pour Tauri 2 et
souvent pour les versions de crates récentes. Utiliser rustup :

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version    # attendu : >= 1.77
```

Pas besoin d'autres targets — on builde nativement.

### 4. Cloner et builder le bench

```bash
git clone git@github.com:hb9tob/NewModem.git ~/NewModem
# ou : git clone https://github.com/hb9tob/NewModem.git ~/NewModem

cd ~/NewModem/rust
cargo build --release --example perf_idle -p modem-core
```

**Durée attendue** :
- Pi 5 (4–8 Go RAM) : ~5–8 min première fois (rustfft, num-complex,
  num-traits, etc.).
- Pi 4 (2–8 Go RAM) : ~15–25 min première fois. Si la machine n'a
  qu'1–2 Go RAM, activer un swap de 2 Go pour éviter l'OOM kill du
  linker. Vérifier `free -h` avant de lancer.

### 5. Stabiliser thermiquement le Pi (important sur Pi 4)

Le Cortex-A72 throttle agressivement à >80 °C. Pour des mesures
reproductibles :

- Vérifier que le Pi a un dissipateur ou un boîtier ventilé.
- Avant de lancer le bench, faire chauffer la machine 30 s avec
  une boucle CPU :
  ```bash
  timeout 30 yes > /dev/null &
  ```
- Lancer le bench juste après, dans un état chaud-stable. Ne pas
  exécuter d'autres charges en parallèle (pas de GUI lourde, pas
  d'apt update en fond).
- Vérifier le throttle pendant le bench :
  ```bash
  vcgencmd get_throttled
  # 0x0 = aucun throttle. Tout autre code = mesure non fiable.
  ```

### 6. Exécution du bench

```bash
cd ~/NewModem/rust
cargo run --release --example perf_idle -p modem-core 2>&1 | tee /tmp/perf-idle-output.txt
```

Le binaire imprime le tableau `detect_best_profile`, `rx_v3_after`
par profil, `legacy idle tick`, `fft probe`, `new idle tick`. **60
itérations**, médiane / p95 / max en µs.

Durée d'exécution du bench lui-même : 1–3 min selon la cible.

### 7. Capture des résultats

Copier la sortie dans `results/` avec une convention de nom alignée
sur l'existant (`results/perf-idle-sp7-{1,2a,2b,2c}.txt`) :

```bash
cd ~/NewModem
mkdir -p results
cp /tmp/perf-idle-output.txt results/perf-idle-${TARGET}-1.txt
# où TARGET = rpi4 ou rpi5
```

Avant de commit, **éditer le haut du fichier** pour ajouter un
en-tête identifiant la machine, par exemple :

```
[bench_rpi] target = Raspberry Pi 5 Model B Rev 1.0
[bench_rpi] cpu    = Cortex-A76 @ 2.4 GHz, aarch64
[bench_rpi] ram    = 8 GiB
[bench_rpi] os     = Raspberry Pi OS 12 (bookworm), kernel 6.6.x
[bench_rpi] rustc  = 1.7x.x
[bench_rpi] throttle_after_run = 0x0  (vcgencmd get_throttled)
[bench_rpi] HEAD   = <git rev-parse --short HEAD>
[bench_rpi] date   = <date -u +%Y-%m-%dT%H:%M:%SZ>

<sortie du binaire ici>
```

### 8. Commit + push

```bash
cd ~/NewModem
git checkout -b perf/rpi-bench-${TARGET}
git add results/perf-idle-${TARGET}-1.txt
git commit -m "perf RPi : bench idle sur ${TARGET}

Mesures 60 itérations, méthodologie identique SP7
(plans/perf-rx-rpi-bench.md). À comparer aux estimations
théoriques du plan."
git push origin perf/rpi-bench-${TARGET}
```

Si les deux Pi sont disponibles, faire **deux branches séparées**
(`perf/rpi-bench-rpi4` et `perf/rpi-bench-rpi5`) — pas un commit
combiné — pour pouvoir comparer les diffs proprement.

---

## Comparaison attendue (à reporter dans le commit message)

Aligner les chiffres sur ce tableau, en mettant un ✓ ou ✗ vs
estimation :

| Métrique | SP7 mesuré | RPi5 estimé | RPi5 mesuré | RPi4 estimé | RPi4 mesuré |
|---|---|---|---|---|---|
| `legacy idle tick` médiane | 1003 ms | ~1500 ms | ? | ~5000 ms | ? |
| `new idle tick` médiane (avec gate) | 1.6 ms | ~2.5 ms | ? | ~9–10 ms | ? |
| `rx_v3_after[Ultra]` médiane | 22 ms | ~35 ms | ? | ~120 ms | ? |

Si un mesuré dévie de plus de **2×** de l'estimation, **flagger**
dans le commit message — peut indiquer un throttle non détecté, une
allocation différente sur ARM, ou un défaut de la lib FFT NEON.

---

## Contraintes dures

1. **Aucune modification de code source**. Pure mesure.
2. **Pas de cross-compile** depuis une autre machine. Build natif sur
   le Pi pour avoir un binaire optimisé pour le SoC réel.
3. **Pas de PGO ni de feature exotique**. `cargo build --release`
   standard, profil release par défaut du workspace.
4. **Pas de `--cfg target_cpu=native`** sauf si le bench montre une
   pénalité majeure et que tu en discutes en commentaire — ça
   compromet la portabilité du binaire et n'est pas représentatif
   d'une release standard.

---

## Si quelque chose dévie

- Build échoue OOM sur Pi 4 → activer 2 Go de swap, retry. Si toujours
  KO, builder uniquement `modem-core` (`cargo build --release -p modem-core --example perf_idle`)
  et pas le workspace complet.
- `vcgencmd get_throttled` ≠ `0x0` après bench → relancer après
  refroidissement. Documenter l'incident en commentaire dans
  l'en-tête du fichier résultat.
- Sortie binaire complètement folle (par ex. plusieurs secondes par
  itération, FFT cassée) → ne pas inventer de fix, **stop et reporter**.
- Si le Pi est en `armv7l` (32-bit) → stopper. Il faut un Pi OS 64-bit
  pour aligner sur la prod (le binaire desktop modem-gui est livré
  ARM64 / x86_64 64-bit uniquement).

---

## Livrable attendu

- Branche `perf/rpi-bench-rpi5` avec `results/perf-idle-rpi5-1.txt`.
- (Si dispo) Branche `perf/rpi-bench-rpi4` avec
  `results/perf-idle-rpi4-1.txt`.
- Commit message court, avec le tableau de comparaison rempli et un
  verdict : "tient le budget 1 s avec X ms de marge" / "throttle suspect",
  etc.

Si à un point tu hésites — surtout sur la stabilité thermique ou un
chiffre qui sort de la fourchette estimée — pose la question avant
de commit.

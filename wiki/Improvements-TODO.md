# Improvements TODO

Liste des améliorations identifiées pour pousser le débit au-delà du
3 kb/s net actuel (Shannon = 10-15 kb/s sur ce canal).

## Faciles (à faire)

- [ ] **Réduire α RRC à 0.10-0.15** — +10-15 % de Rs pour un filtre plus long
- [ ] **Baisser amplitude pilotes à -12 dB** — récupère 1-2 dB de SNR data
- [ ] **Ajouter un égaliseur DFE/LMS** — débloque 1000 Bd pour 16QAM/32QAM
- [x] **Pilotes TDM au lieu de CW** — supprime la collision pilote-data
- [x] **DFE 9+5 taps** — gain en sim, mais tombe en panne OTA (error propagation)
- [x] **LDPC WiMAX (IEEE 802.16) soft-decision** — 4.2 kb/s net en sim,
      OTA a valider

## Moyennes

- [x] **LDPC soft-decision** — fait avec WiMAX 802.16 codes (3/4 et 1/2)
- [ ] **Bit-interleaved coded modulation (BICM)** pour 32QAM — approche Shannon
- [ ] **Timing dynamique dans le demod** — permet blocs >15 s malgré drift 16 ppm
- [ ] **Soft-clipping dans le sim TX** — modéliser plus finement le limiteur réel
      (aujourd'hui hard-clip a 0.55, OTA suggère soft-clip plus sévère)

## Difficiles

- [ ] **CPM/CPFSK à enveloppe constante** — contourne le hard-clip TX,
      potentiel 6-8 kb/s avec 8-FSK
- [ ] **Turbo-équalisation** (égaliseur + LDPC itératif) — +2-3 dB supplémentaires
- [ ] **Ré-écriture GNU Radio en blocs OOT C++** pour le déploiement en direct

## Notes de design

- Le canal limite est à ~1900 Hz utile (HPF CTCSS 300 Hz + LPF audio 2200 Hz)
- Phase non-linéaire 40° std dans la bande — principale limite pour QAM denses
- Dérive 16 ppm mesurée, jusqu'à 200 ppm possible selon transceiver
- Hard-clip TX à ~-6 dB (peak 0.45) révélé par balayage OTA

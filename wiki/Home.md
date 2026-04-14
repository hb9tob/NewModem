# NewModem wiki

Modem audio pour transmission d'images via relais NBFM radio amateur.
Conception, caractérisation de canal, simulateur et bancs de test.

## Sommaire

- **[Channel Characterization](Channel-Characterization)** — mesures du canal NBFM réel (voice et data mode), réponse en amplitude et phase, dérive d'horloge
- **[NBFM Channel Simulator](NBFM-Channel-Simulator)** — simulateur GNU Radio du canal, calibré sur les mesures
- **[Modem Design](Modem-Design)** — choix single-carrier, placement des pilotes, dual pilotes 400/1800 Hz
- **[BER Benchmarks](BER-Benchmarks)** — BER 8PSK / 16QAM / 32QAM vs symbol rate
- **[Scripts Reference](Scripts-Reference)** — comment lancer chaque script

## Objectifs du projet

Construire un modem **broadcast (point-multipoint, sans ARQ)** capable de
transmettre des images à travers les relais NBFM voix. Contraintes :

- Ouvert (pas de VARA)
- Resynchronisation rapide (QRM, nouveau receiver joignant en cours)
- PAPR faible pour survivre au limiteur audio en mode voice
- Débit cible : 3-4 kb/s net, au-delà de ce que HamDRM+turbo donne

## État d'avancement

- [x] Caractérisation canal NBFM (voice + data mode)
- [x] Simulateur canal GNU Radio validé (écart <1 dB avec mesures)
- [x] Étude placement pilote (single + dual)
- [x] Banc BER 8PSK/16QAM/32QAM
- [ ] Pilotes TDM (pour pousser au-delà de 1000 Bd)
- [ ] FEC LDPC short frames (DVB-S2X VL-SNR)
- [ ] Prototype broadcast complet

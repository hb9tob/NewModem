NBFM Modem GUI — version portable Windows
============================================

Decompresse ce zip ou tu veux (ex: D:\NbfmModem\). Aucune installation,
aucune modification du registre. Tout le state (parametres, captures
recues, sessions decodees) reste dans le sous-dossier "data\" cree au
premier lancement, a cote de l'executable.

Pour deplacer ou archiver une instance, copie simplement le dossier
entier — tu emportes ton historique avec toi.

Lancement
---------
Double-clique nbfm-modem-gui.exe.

Prerequis
---------
WebView2 Runtime (composant Microsoft Edge) doit etre present.
- Windows 11 et Windows 10 >= 1803 (avril 2018) : deja installe.
- Versions plus anciennes : telecharger l'installeur "Evergreen Bootstrapper" :
  https://developer.microsoft.com/microsoft-edge/webview2/

Aucune autre dependance (pas de Visual C++ Redistributable a installer).

Contenu du dossier
------------------
nbfm-modem-gui.exe                          GUI Tauri (point d'entree)
nbfm-modem-x86_64-pc-windows-msvc.exe       Modem CLI (sidecar appele par le GUI)
portable.txt                                Marqueur du mode portable —
                                            ne pas supprimer, sinon le GUI
                                            stocke a nouveau sous %APPDATA%
README-portable.txt                         Ce fichier
data\                                       Cree au 1er lancement, contient :
  settings.json                             Parametres GUI persistes
  nbfm-rx\                                  Images / fichiers recus
  nbfm-rx\sessions\                         Sessions RaptorQ en cours / decodees

Soumission au collecteur de canal
---------------------------------
Cette version porte la cle HMAC partagee avec le serveur newmodem-collector.
Renseigne ton indicatif et l'URL du collecteur dans Parametres, puis valide
une capture brute via l'onglet Canal. Tes sondages alimentent la cartographie
publique du canal NBFM radioamateur — merci de contribuer.

Pas de telemetrie automatique : aucun envoi sans clic explicite sur
"Soumettre la capture".

Desinstallation
---------------
Supprime le dossier. Rien d'autre a nettoyer.

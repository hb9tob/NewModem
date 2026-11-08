# Relicensing consent — contributor request

**Contributor** : GitHub `@Serphentas` (git author `xerxes <xerxes@gulftown.localdomain>`)
**Pull request** : [#2](https://github.com/hb9tob/NewModem/pull/2) — *Enable AppImage builds & require callsign before TX* (état : CLOSED)
**Commits concernés** : `cbb5de6`, `5b074c1`
**Statut** : à envoyer — en attente de réponse
**Pourquoi** : le projet n'avait aucune licence au moment de sa contribution,
il a donc conservé le droit d'auteur sur ses lignes. Quatre fichiers sont
intégralement de lui et n'ont jamais été modifiés depuis
(`docker/appimage.Dockerfile`, `scripts/fix_appimage_egl.sh`, `Makefile`,
`.dockerignore`). Voir la section 4 de `NOTICE`.

---

## Message à poster (commentaire sur la PR #2)

> Hi @Serphentas,
>
> I'm putting NewModem under a licence — it never had one, which I now realise
> means nobody could legally fork it, yourself included. The project is going
> **GPL-3.0-or-later** (with AGPL-3.0-or-later for the `newmodem-collector`
> crate, which is a hosted HTTP service). The goal is for the project and all
> its forks to stay open.
>
> Your two commits from PR #2 predate that licence, so you still hold the
> copyright on them:
>
> - `cbb5de6` — *feat(build): add portable Linux AppImage*
> - `5b074c1` — *fix(gui): require callsign before transmission*
>
> Four of those files are entirely yours and are still in the tree exactly as
> you wrote them: `docker/appimage.Dockerfile`, `scripts/fix_appimage_egl.sh`,
> `Makefile` and `.dockerignore`.
>
> **Could you confirm in this thread that you agree to license your
> contributions to NewModem under GPL-3.0-or-later?** A single line is enough,
> for example:
>
> > I agree to license my contributions to NewModem (commits cbb5de6 and
> > 5b074c1) under GPL-3.0-or-later.
>
> If for any reason you'd rather not, that's completely fine — just say so and
> I'll reimplement that build tooling from scratch, no hard feelings. I'd just
> rather ask than assume.
>
> Going forward the project uses a DCO (`git commit -s`) rather than a CLA, so
> contributors keep their copyright — see the new `CONTRIBUTING.md`.
>
> Thanks again for the AppImage work, it's genuinely useful — it's shipping in
> 0.20.1 and it's what makes the Linux build portable.
>
> 73,
> Olivier / HB9TOB

---

## Commande

```bash
gh pr comment 2 --body-file - <<'EOF'
<coller le corps du message ci-dessus, sans les "> ">
EOF
```

## Après réception de l'accord

1. Supprimer la section 4 de `NOTICE`.
2. Supprimer l'encadré ⚠ des deux sections Licence/License du `README.md`.
3. Ajouter les en-têtes SPDX dans tous les fichiers source (passe unique,
   désormais sans risque de surdéclaration).
4. Noter ici la date et le lien du commentaire d'accord.

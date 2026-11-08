# Contributing to NewModem

Thanks for your interest. A few things to know before you send a patch.

## Licence and the DCO

NewModem is **GPL-3.0-or-later** (the `newmodem-collector` crate is
**AGPL-3.0-or-later**). By contributing, you agree to license your contribution
under the same terms.

This project uses a **Developer Certificate of Origin (DCO)**, not a Contributor
License Agreement. You keep the copyright on your contribution; you are not
assigning anything to anyone. That is the point: because the copyright stays
distributed among contributors, **nobody — not even the lead author — can
relicense the project into a closed one later**. A CLA with copyright
assignment is exactly how open projects get closed down, so we do not use one.

Certify your contribution by signing off each commit:

```bash
git commit -s -m "fix(gui): …"
```

which appends:

```
Signed-off-by: Your Name <your.email@example.com>
```

By signing off you certify the [DCO 1.1](https://developercertificate.org/):
that you wrote the patch, or have the right to submit it under the project's
licence.

## Language

Set in `CLAUDE.md` and enforced in review:

- **Code, comments, doc-strings, log messages, identifiers, commit messages and
  PR descriptions: English.**
- **User-facing UI strings: French** (button labels, hints, GUI error messages),
  because the target audience is French-speaking radio amateurs. GUI strings
  live in `rust/modem-gui/src/i18n/{fr,en}.json` — add the key to **both**.

## Commits

Conventional-commit prefixes, as in the existing history:

```
feat(scope): …    fix(scope): …    perf(scope): …
refactor(scope): …    docs(scope): …    chore(scope): …
```

Keep one logical change per commit.

## Building

The workspace lives under `rust/`. On Debian-family hosts the system
`/usr/bin/cargo` may be too old (the `raptorq` dependency needs Rust ≥ 1.87),
so use the rustup toolchain explicitly:

```bash
~/.cargo/bin/cargo build --release -p modem-cli
~/.cargo/bin/cargo build --workspace          # full check
```

Before sending a patch, make sure `cargo build --workspace` is clean and that
you have not added new warnings.

## Scope of a patch

The modem is a protocol implementation with peers in the field: a change to the
wire format, the superframe layout, the profile table or the marker breaks
interoperability with every station running a released version. If your patch
touches any of those, say so explicitly in the PR description and expect a
discussion about versioning before it is merged.

## Third-party native libraries

Any new native dependency must be GPL-compatible. `NOTICE` lists the current
set and the one exception (the proprietary SDRplay API, covered by an
additional permission under GPLv3 section 7, and loaded via `dlopen` so it is
never bundled). Do not add a dependency that would require relicensing the
project.

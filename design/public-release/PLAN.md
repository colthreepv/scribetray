# Scribetray — public release plan

Goal: publish the repository on GitHub with a README that explains what Scribetray is and how to try it. Automated builds and code signing are a separate, later milestone (M2).

Draft README: `README.draft.md` in this folder. Banner: `media/banner.png`, 1280×640, which is also the GitHub social preview. Generator: `tools/render_banner.py`.

## M1: clean up and publish

Audit already done (2026-09-28): the tracked files contain no private paths or credentials. `git log --all -p` shows no API keys, and every commit has a single author (`Valerio Coltre <valerio.coltre@gmail.com>`). `AGENTS.md` is a Git-ignored symlink to private notes and stays out. Publishing the full history (19+ commits) is fine; don't squash.

### 1. Repository layout

- Move `design/ui-refresh/out/tray/*.ico` to `assets/tray/` and update the paths in `build.rs`. The build must not depend on the design folder.
- Replace `assets/scribetray.ico` with `design/ui-refresh/out/scribetray.ico` if they differ.
- Create `docs/media/` with the README images: `banner.png`, `overlay-recording.gif`, `tray-states.png`, and a real screenshot of the right-click menu (see step 4).
- Delete `design/` from the published tree, since it only holds past plans and generators. Keep the generators if you want: move `design/*/tools/*.py` to `tools/design/`. Delete `docs/m0-validation.md`; it's an internal test log.
- `.gitignore`: keep the `AGENTS.md` entry.

### 2. Documentation

- Replace `README.md` with the draft. Fix the image paths to `docs/media/…`, and keep `../../releases/latest` (relative links work on GitHub).
- Move the developer-oriented content from the current README into `docs/development.md`: `deploy-latest.ps1` usage and `deploy.json`, realtime segmenting and batch fallback, the versioning scheme, caret detection notes, and autostart path stability. Don't carry over the "Implementation status" section.
- Add `LICENSE` (MIT, © 2026 Valerio Coltre) and `license = "MIT"`, `repository`, `readme`, `keywords` (`dictation`, `speech-to-text`, `elevenlabs`, `windows`, `tray`) in `Cargo.toml`.
- Add `CHANGELOG.md`, one short paragraph per minor version from the git log (v0.1 to the current version).
- Check before publishing: the free-plan allowance (10,000 credits/month) and the $0.22/hour pay-as-you-go price on [elevenlabs.io/pricing](https://elevenlabs.io/pricing). The README deliberately gives no hour count for the free plan, because credits per hour differ by plan (this pay-as-you-go account measured about 585 credits per hour). Add an hour figure only if ElevenLabs states one for API usage on the free plan.
- Check that every setting in the README's TOML block exists in `config.rs` with that default.

### 3. First-run experience (small code changes that make the README true)

- A fresh install without a key shows the faded icon and "Set API key…" as the first menu item. Confirm that this works on a clean profile: rename `%APPDATA%\Scribetray` and `%LOCALAPPDATA%\Scribetray` first.
- On first launch without a key, show one balloon: "Scribetray needs an ElevenLabs API key. Right-click the tray icon → Set API key…".
- The generated `config.toml` should include the comments from the README block, so the file explains itself.

### 4. Publish

1. Take the menu screenshot on a clean desktop at 100 % scaling, with the usage header visible. Save it as `docs/media/menu.png` and add it under "Everyday use".
2. Build `cargo build --release --locked` and smoke-test the exe on the clean profile.
3. Create the repo with the public persona: `gh repo create colthreepv/scribetray --public --description "Speak into any text field on Windows. Tray dictation powered by ElevenLabs Scribe." --homepage ""`. Add topics: `dictation speech-to-text elevenlabs windows rust tray-app voice-typing`.
4. Add `github` as a second remote and push `main` plus tags. Gitea stays `origin`.
5. Upload `docs/media/banner.png` as the social preview (Settings → General). The GitHub CLI can't do this.
6. Create release `v0.x.y` manually. Attach `scribetray-v0.x.y-x86_64.zip` (the exe only) and `SHA256SUMS.txt`. Release notes: the CHANGELOG entry plus the unsigned-binary note from the README.

Accept: a stranger can go from the README to a working dictation using only the README.

## M2: automated builds (later)

For planning only; don't start with M1.

- GitHub Actions on `windows-latest`: `cargo fmt --check`, `clippy -D warnings`, and `test` on every push; a release build on `v*` tags that uploads the zip, SHA-256 sums, and a build provenance attestation (`actions/attest-build-provenance`).
- Signing options, cheapest first: none (document SmartScreen, as in M1); [SignPath Foundation](https://signpath.org/) (free for open-source projects, requires an application and signs in CI); Azure Trusted Signing (a monthly fee, and requires identity verification). SmartScreen reputation builds over downloads even when signed.
- Optional: a winget manifest (`winget-pkgs`) once releases are automated.
- Version stamping: the tag must match `Cargo.toml`; fail the job otherwise.

## Decisions to confirm

1. License: MIT is proposed. Say so if you prefer `MIT OR Apache-2.0` (common in Rust) or something else.
2. The 🎙️ prefix stays on by default and is presented as a feature. Turning it off by default is the alternative.

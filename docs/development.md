# Development

For installation and daily use, see the [README](../README.md). The full
configuration reference is in [Settings](settings.md).

## Native build tools

Build on Windows with Rust 1.98 or newer, the MSVC C++ build tools, CMake,
NASM and libclang. The OpenAI client uses `wreq` and BoringSSL, which need
these native tools. Downloaded executables do not need them.

For a one-off build, install the tools and run:

```powershell
. .\scripts\native-tools.ps1
cargo build --release --locked
cargo test --locked
```

The helper checks `PATH`, then `%USERPROFILE%\bin\cmake\bin\cmake.exe`
and `%USERPROFILE%\bin\nasm\nasm.exe`. It looks for `libclang.dll` in
`%USERPROFILE%\bin\libclang`. Override those paths with `CMAKE`,
`CMAKE_ASM_NASM_COMPILER` and `LIBCLANG_PATH`.

For plain Cargo commands and rust-analyzer, put your installed paths in
`%USERPROFILE%\.cargo\config.toml` instead:

```toml
[env]
LIBCLANG_PATH = 'C:\Users\<you>\bin\libclang'
CMAKE = 'C:\Users\<you>\bin\cmake\bin\cmake.exe'
CMAKE_ASM_NASM_COMPILER = 'C:\Users\<you>\bin\nasm\nasm.exe'
ASM_NASM = 'C:\Users\<you>\bin\nasm\nasm.exe'
```

The repository's `.cargo/config.toml` links the C/C++ runtime statically on
`x86_64-pc-windows-msvc`. Use `dumpbin /dependents` on the release executable
to check that it imports only Windows system DLLs. Keep `Cargo.lock` committed
and use `--locked`. There is no hosted build CI.

## Local deployment

```powershell
.\scripts\deploy-latest.ps1 -KeepBuilds 5
& "$env:LOCALAPPDATA\Scribetray\builds\latest\scribetray.exe"
```

The helper builds a release, records its commit and checksum in `build.json`,
and switches the `latest` junction to the new build. Restart Scribetray to
load it. By default it keeps the newest three builds, plus any older build
that is still running.

Save defaults in `%APPDATA%\Scribetray\deploy.json`:

```json
{
  "buildRoot": "D:\\Apps\\Scribetray\\builds",
  "keepBuilds": 5,
  "target": "x86_64-pc-windows-msvc",
  "cargoArgs": ["--jobs", "4"]
}
```

Command-line values override `SCRIBETRAY_BUILD_ROOT`, `SCRIBETRAY_KEEP_BUILDS`
and `SCRIBETRAY_TARGET`, which override the JSON file. `-CargoArgs` overrides
the saved `cargoArgs`.

The Start with Windows option stores the current executable path in the user's
Run registry key. If you move the executable, disable and re-enable that option.

## Release procedure

1. Set Cargo to the intended stable version, then commit the complete source.
2. Build a candidate from a clean checkout:

   ```powershell
   .\scripts\deploy-latest.ps1 -CandidateLabel '0.9.0-test.1'
   ```

3. Restart Scribetray and test that candidate. The tray reads its label from
   `build.json`; Cargo and the executable retain version `0.9.0`. The helper
   also saves the candidate in `dist\candidates\0.9.0-test.1`. If code changes,
   commit it and increment the test number.
4. After the user approves the candidate, tag its commit and push to Gitea:

   ```powershell
   git tag -a v0.9.0 -m "Scribetray v0.9.0" <approved-commit>
   git push origin main refs/tags/v0.9.0
   ```

5. Verify that the GitHub mirror has the same commit and tag. Prepare the
   release with a short description of changes since the previous stable tag:

   ```powershell
   .\scripts\release.ps1 -CandidateLabel '0.9.0-test.1' -Summary 'Describe the changes here.'
   ```

6. Review `dist\releases\v0.9.0\RELEASE_NOTES.md` and the packaged files, then
   publish:

   ```powershell
   .\scripts\release.ps1 -CandidateLabel '0.9.0-test.1' -Action Publish
   ```

Use the version being released in these examples. Do not push application
changes or tag a release before the user approves the tested candidate.
The release helper checks commits and checksums, then packages the exact
approved executable. Gitea hosts source and tags; GitHub hosts the executable,
ZIP and `SHA256SUMS.txt`. The [notes template](release-notes-template.md)
supplies the download instructions.

## Transcription

OpenAI starts a WebSocket at recording start and sends 16 kHz mono PCM16 audio.
It uses the existing Codex login without changing or renewing it. Only the
finished transcript reaches the input field, after stop. A connection, protocol
or finalization failure triggers one batch retry through the same provider.
Cancel aborts without a retry.

ElevenLabs uses batch transcription by default. With `realtime = true`, it
streams audio and commits segments about every 25 seconds, then joins the
transcript after stop. A failed stream falls back to batch. `keyterms` apply
only to ElevenLabs streaming.

History keeps the original audio. `trim_silence` applies only to batch uploads,
including fallback. Streaming sends untrimmed audio. Cancel cannot retract audio
already sent to either provider.

Unit tests make no provider requests. For a live check, supply 16 kHz mono
signed-16 little-endian PCM:

```powershell
cargo run --release --locked --example openai-stream-smoke -- stream sample.pcm
cargo run --release --locked --example transcribe-smoke -- elevenlabs sample.pcm
```

The OpenAI example also accepts `batch`, `trimmed` and `cancel`. The ElevenLabs
example reads `ELEVENLABS_API_KEY`. These commands send the supplied audio to
the selected provider.

## Text insertion

Recording captures the foreground window and focused control. The overlay uses
the caret reported by Win32, Microsoft Active Accessibility or UI Automation,
with the mouse position as a fallback.

Before insertion, Scribetray checks that the original target still has focus.
If focus moved, it copies the text to the clipboard and waits up to 20 seconds.
Returning to the target for 50 ms allows insertion. A new recording clears
the pending insertion; expiry leaves the text on the clipboard.

Type mode sends Unicode keyboard input. Paste mode temporarily changes the
clipboard and can restore its previous contents. It preserves clipboard data
changed by another app during the paste. Check text insertion in browser
editors and terminals when changing this code, since their focus and caret
reporting vary.

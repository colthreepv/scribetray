# Scribetray

Scribetray is a Windows tray dictation app. Press a global hotkey, speak, and
let ElevenLabs Scribe insert the transcript into the field that had focus when
recording began.

## Build and run

Requirements: Rust 1.85 or newer and the Windows MSVC toolchain.

```powershell
cargo build --release
.\target\release\scribetray.exe
```

The release executable embeds the Scribetray icon and a PerMonitorV2,
`asInvoker` manifest. It creates its configuration on first launch at
`%APPDATA%\Scribetray\config.toml`; logs and the last 20 recordings are stored
under `%LOCALAPPDATA%\Scribetray`.

## Use

- **Win+Alt+V** starts or stops recording. **Esc** cancels the current
  recording.
- **Win+Alt+Shift+V** records and submits with Enter.
- The tray menu controls the emoji prefix, Auto-Enter, sound cues, typing mode,
  language, Start with Windows, and recording history.
- **Settings and hotkeys…** opens the TOML config. Changes to hotkeys, model,
  microphone, language, or recording length take effect after restarting the
  app. Supported hotkey keys are letters, digits, F1–F24, Space, Enter, Tab,
  and Esc, with Win, Alt, Ctrl, and Shift modifiers.
- Failed uploads keep the PCM recording in history for retry. A focus change
  before insertion sends the transcript to the clipboard instead.

The app reads `ELEVENLABS_API_KEY` first, then `api_key` from the config file.
`tag_audio_events=false` is sent with each Scribe request.

## Implementation status

The Rust M1/M2 application builds and starts on Windows. Its startup log
confirms registration of the default recording and submit hotkeys. It includes
WASAPI-backed capture through CPAL, Scribe transcription, a non-activating
recording anchor, clipboard-preserving paste, guarded Unicode typing, local
history, retry, language selection, optional Enter, and per-user autostart.

The M0 checks identified an outstanding Codex App limitation: the editor's
focused element and RuntimeId are available, but the tested Win32, MSAA, and
UIA caret APIs did not return a caret rectangle, so the anchor uses the mouse
position fallback there. The complete caret and paste round-trip matrix across
Codex App, Chrome, VS Code, Notepad, and Windows Terminal still needs a manual
pass. Real-time streaming and push-to-talk remain future work.

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
  recording. The tray menu can switch to push-to-talk, where holding the
  configured toggle chord records until you release it.
- **Win+Alt+Shift+V** records and submits with Enter.
- The tray menu can capture a new toggle hotkey and controls push-to-talk,
  realtime transcription, microphone selection, the emoji prefix, Auto-Enter,
  sound cues, typing mode, language, Start with Windows, and recording history.
- **Open settings file…** opens the TOML config. Settings reload automatically
  after saving the file, once any active recording ends. Supported hotkey keys
  are letters, digits, F1–F24, Space, Enter, Tab, and Esc, with Win, Alt, Ctrl,
  and Shift modifiers.
- Failed uploads keep the PCM recording in history for retry. A focus change
  before insertion sends the transcript to the clipboard instead.

The app reads `ELEVENLABS_API_KEY` first, then `api_key` from the config file.
Batch Scribe requests send `tag_audio_events=false`. Realtime mode opens a
`scribe_v2_realtime` WebSocket when recording starts and commits the final audio
chunk on stop. During longer recordings it commits segments every 25 seconds and
joins their committed text, avoiding ElevenLabs' automatic segment boundary. If
that session fails, Scribetray retries the complete saved audio through the batch
`scribe_v2` endpoint. Realtime mode is off by default; turn it on in the tray
menu to use it. Optional `keyterms` in the TOML config are sent as Scribe
vocabulary hints. The websocket uses ElevenLabs'
[realtime API](https://elevenlabs.io/docs/api-reference/speech-to-text/v-1-speech-to-text-realtime);
the API reference documents a 20% transcription premium for non-empty keyterm
lists, so the default list is empty. New installations use Type mode by
default: it simulates Unicode keystrokes and avoids relying on the clipboard.
Use Paste mode from the tray menu for a field that handles paste better.

Example settings:

```toml
hotkey = "Win+Alt+V"
hotkey_submit = "Win+Alt+Shift+V"
mode = "toggle" # or "push_to_talk"
realtime = false
keyterms = []
insert_method = "type" # or "paste"
```

## Implementation status

The Rust application builds for `x86_64-pc-windows-msvc`; the M1/M2 startup log
confirms registration of the default recording and submit hotkeys. The user
has confirmed a realtime recording, transcription, and insertion round-trip in
Codex App. Push-to-talk and the full target-app matrix still need a manual pass.
The app includes
WASAPI-backed capture through CPAL, batch and realtime Scribe transcription,
push-to-talk, a configurable toggle hotkey, a non-activating recording anchor,
clipboard-preserving paste, guarded Unicode typing, local history, retry,
language selection, optional Enter, and per-user autostart.

Earlier Codex App logs reported the mouse-position fallback; updated runs have
reported both UIA `TextPattern` caret detection and mouse fallback. The user
confirmed that a 46-second Realtime recording retained the full message; a
local WebSocket test also verifies joining multiple committed segments.
The user confirmed Auto-Enter works in Codex App and reported no noticeable
speed penalty with Type mode. New installations now default to Type mode; the
complete caret and insertion compatibility matrix across Codex App, Chrome,
VS Code, Notepad, and Windows Terminal remains to be verified. Windows Terminal
is a secondary compatibility check, not a primary dictation workflow.

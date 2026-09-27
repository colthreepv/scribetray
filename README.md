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

- **Win+Alt+V** starts or stops recording. While recording, **Enter** stops,
  inserts the transcript, and submits it once. **Esc** cancels the current
  recording. The tray menu can switch to push-to-talk, where holding the
  configured toggle chord records until you release it.
- Left-clicking the system-tray icon copies the newest recoverable dictation;
  right-click opens the full menu.
- The tray menu can capture a new toggle hotkey and controls push-to-talk,
  realtime transcription, microphone selection, the emoji prefix, Auto-Enter,
  sound cues, typing mode, language, Start with Windows, and recording history.
- **Open settings file…** opens the TOML config. Settings reload automatically
  after saving the file, once any active recording ends. Supported hotkey keys
  are letters, digits, F1–F24, Space, Enter, Tab, and Esc, with Win, Alt, Ctrl,
  and Shift modifiers.
- Failed uploads keep the PCM recording in history for retry. A focus change
  before insertion sends the transcript to the clipboard instead.
- History lists the 10 newest recoverable dictations with a transcript preview
  and duration. A warning marks text that was copied instead of inserted;
  failed recordings can be retried, and pending recordings are grayed out.
- While recording, a compact non-activating pill follows the caret. Its waveform
  reacts to the live microphone level; the tray icon and tooltip show recording,
  transcription, setup, and error states.

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
mode = "toggle" # or "push_to_talk"
realtime = false
keyterms = []
insert_method = "type" # or "paste"
```

## Versioning

Release versions follow the implementation milestones: M0 was a disposable
spike, M1 maps to `v0.1.x`, M2 to `v0.2.x`, M3 to `v0.3.x`, and the recovery
and send refresh is `v0.4.x`. The current build is `v0.4.0`; patch numbers
increase for fixes within the current milestone. The tray tooltip shows the
running version.

Scribetray is a per-user desktop application and needs no installer or
administrator rights. It can run directly from this repository's
`target/release/scribetray.exe`; settings and history are stored in the user's
Windows profile. “Start with Windows” launches the executable path saved at
the time it is enabled, so keep that path stable while using autostart. There
is no Windows Service mode; Scribetray needs the interactive desktop for its
tray icon, global hotkeys, microphone, and text-field insertion.

## Implementation status

The Rust application builds for `x86_64-pc-windows-msvc`; the UI registers the
configured recording hotkey and registers Enter and Esc only while recording.
The user has confirmed a realtime recording, transcription, and insertion
round-trip in Codex App. Push-to-talk and the full target-app matrix still need
a manual pass.
The app includes
WASAPI-backed capture through CPAL, batch and realtime Scribe transcription,
push-to-talk, a configurable toggle hotkey, a non-activating recording anchor,
clipboard-preserving paste, guarded Unicode typing, local history, retry,
language selection, Enter-to-send while recording, optional Auto-Enter on stop,
and per-user autostart.

Earlier Codex App logs reported the mouse-position fallback; updated runs have
reported both UIA `TextPattern` caret detection and mouse fallback. The user
confirmed that a 46-second Realtime recording retained the full message; a
local WebSocket test also verifies joining multiple committed segments.
The user confirmed Auto-Enter works in Codex App and reported no noticeable
speed penalty with Type mode. New installations now default to Type mode; the
complete caret and insertion compatibility matrix across Codex App, Chrome,
VS Code, Notepad, and Windows Terminal remains to be verified. Windows Terminal
is a secondary compatibility check, not a primary dictation workflow.

The tray menu groups recording and insertion settings, uses radio choices for
mutually exclusive modes, shows the configured hotkeys, and previews recent
history entries. Tray icons adapt to both app state and Windows taskbar theme.
The overlay uses a DPI-scaled alpha-rendered waveform and reads the recorder's
non-blocking live level meter.

The latest desktop feedback reports that message-beep sound cues were inaudible,
and that Paste mode can leave the clipboard replaced and skip Auto-Enter. The
app now uses embedded WAV cues and restores the saved clipboard when the
temporary transcript is still present, while preserving newer clipboard data.
Right-click opens the tray menu; left-click copies or retries the newest
recoverable dictation. Sound output and Chrome Paste-mode Auto-Enter still need
a retest with the updated executable.

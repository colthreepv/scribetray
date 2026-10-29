<p align="center">
  <img src="docs/media/banner.png" alt="Scribetray: speak into any text field on Windows" width="100%">
</p>

<p align="center">
  <b>Press a hotkey, talk, and your words appear where your cursor is.</b><br>
  A tiny Windows tray app using ElevenLabs Scribe or your local Codex ChatGPT session.
</p>

<p align="center">
  <img alt="Windows 10/11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?logo=windows&logoColor=white">
  <img alt="Written in Rust" src="https://img.shields.io/badge/Rust-1.98%2B-B7410E?logo=rust&logoColor=white">
  <img alt="License: WTFPL" src="https://img.shields.io/badge/license-WTFPL-2EA44F">
</p>

---

Typing a long prompt to an AI agent, answering a chat, writing a commit message: most of what we type we could just *say*. Scribetray lets you do that in any app, without switching windows or copying anything.

1. Put your cursor in a text field.
2. Press **Win+Alt+V** and speak. A small waveform floats next to your cursor so you know it's listening.
3. Press **Win+Alt+V** again. A moment later the text is typed in, right where you left it.

Press **Enter** instead to insert the text *and send it*, which is handy in chat boxes and AI prompts. Press **Esc** to throw the recording away.

<p align="center">
  <img src="docs/media/overlay-recording.gif" alt="The recording overlay: a small pill with a live waveform" width="330">
</p>

## Why Scribetray

- **Works everywhere you type.** Browsers, Electron apps, editors, chat clients. Scribetray types the text as keystrokes into the field that had focus when you started, so it doesn't depend on per-app integrations.
- **Your clipboard stays yours.** Text is typed, not pasted. A paste mode exists for the rare field that prefers it, and it restores your clipboard afterwards.
- **Nothing gets lost.** If you switched windows while it was transcribing, the text goes to the clipboard instead of into the wrong app. The last recordings stay on disk, so a failed upload can be retried and **one click on the tray icon** copies your last dictation.
- **Honest about what it did.** By default every dictation starts with 🎙️, so whoever reads it (a colleague, or an AI agent) knows it was spoken and may contain a transcription slip. You can turn this off in the menu.
- **Accurate in many languages.** Scribe auto-detects the language, or you can pin one from the menu.
- **Small and quiet.** A single native executable. No installer, no admin rights, no background service, no telemetry.

## Get started

### 1. Get an ElevenLabs API key

Scribetray uses your own ElevenLabs account, so there is no subscription to Scribetray itself.

1. [Sign up at ElevenLabs](https://elevenlabs.io/app/sign-up). The free plan includes 10,000 credits a month, enough to try Scribetray properly.
2. Open **Developers → API Keys → Create API key**.
3. Turn on **Restrict key** and enable only:
   - **Speech to Text**, required.
   - **User → Read**, optional. It lets the tray menu show how much of your monthly allowance you've used.

### 2. Download and run

Download `scribetray.exe` from the [latest GitHub release](https://github.com/colthreepv/scribetray/releases/latest) and put it somewhere permanent, for example `%LOCALAPPDATA%\Programs\Scribetray\`. Then run it.

> [!NOTE]
> The executable isn't code-signed yet, so Windows SmartScreen may warn you the first time. Choose **More info → Run anyway**. You can also [build it yourself](#build-from-source).

### 3. Add your key

The tray icon appears faded until Scribetray has a key. Right-click it, choose **Set API key…**, and paste the key into `api_key = "..."`. Save the file and you're done; Scribetray picks up the change immediately.

Alternatively, set the `ELEVENLABS_API_KEY` environment variable.

### Use your Codex ChatGPT session instead

1. Sign in to Codex app or Codex CLI with your ChatGPT account on this computer.
2. Open Scribetray's settings file from the tray menu and add `provider = "openai"`.
3. Save the file. Scribetray detects the session and shows the account and cached
   plan in the menu. No ElevenLabs API key is needed for this provider.

Scribetray reads `auth.json` inside `CODEX_HOME` when set, otherwise
`%USERPROFILE%\.codex\auth.json`. An invalid explicit `CODEX_HOME` reports an
error instead of selecting a different account. This version supports Codex's
file-based ChatGPT session; API-key sign-in and OS credential-store-only
sessions are not supported by this integration.

OpenAI dictation uploads the recording when you stop. The realtime option is
hidden while OpenAI is selected; its saved ElevenLabs preference is preserved.
There is no usage meter for OpenAI. The plan label is cached session metadata,
not a live billing check or a guarantee of access or unlimited transcription.

Scribetray never renews the session or changes Codex's credentials. If it expires,
open Codex and use or sign in to it so Codex can renew the session, then try
again. Scribetray rereads the file without requiring a restart.

This experimental integration uses the private ChatGPT dictation endpoint used
by the desktop app. It may change or become unavailable. It does not use a paid
OpenAI Platform API key. Audio follows ChatGPT's service and retention rules;
Scribetray does not delete server-side uploads.

## Everyday use

| Key | While idle | While recording |
|---|---|---|
| **Win+Alt+V** | Start recording | Stop and insert |
| **Enter** | (untouched) | Stop, insert, and send, when Enter interception is enabled |
| **Shift+Enter** | (untouched) | Insert a line break; keep recording |
| **Esc** | (untouched) | Cancel |

**The tray icon** shows what's happening: a coral wave when ready, blue while recording, amber while transcribing, and a red × if something failed.

<p align="center">
  <img src="docs/media/tray-states.png" alt="Tray icon states on dark and light taskbars" width="520">
</p>

- **Left-click** the icon to copy your last dictation, or to retry it if it failed.
- **Right-click** for the menu. From there you can choose the microphone and language, browse recent dictations, switch to push-to-talk, and change the hotkey. It also shows your ElevenLabs usage for the month.

The tray menu includes **Intercept Enter while recording**. With it enabled,
plain Enter stops the recording and sends the transcript, while Shift+Enter
reaches the focused app and the recording continues. Turn the option off to
let both Enter combinations reach the app. Use the recording hotkey to stop
and insert the transcript.

With ElevenLabs selected, the tray menu shows cached monthly usage and an estimate of remaining batch transcription time. Set `scribe_credits_per_hour` to tune that estimate; realtime usage is excluded. With OpenAI selected, it shows the detected account and optional plan instead.

Scribetray listens only while recording. In realtime mode, audio streams to ElevenLabs as you speak; in batch mode, it is uploaded when you stop. Esc is claimed only while a recording is running. Plain Enter is intercepted only when **Intercept Enter while recording** is enabled; Shift+Enter always reaches the focused app.

## What does it cost?

Scribetray is free and open source. ElevenLabs transcription uses your ElevenLabs account and its current [pricing](https://elevenlabs.io/pricing). The OpenAI provider uses your existing Codex ChatGPT session and the access available to that account. Scribetray cannot report its remaining transcription allowance.

## Privacy

- Audio is sent to your selected provider when you record. ElevenLabs realtime streams while you speak; both batch providers upload when you stop. See the selected provider's privacy and retention policies.
- Your last 20 recordings (audio and text) are kept on your PC in `%LOCALAPPDATA%\Scribetray\history`, so that nothing is lost when something fails. Delete that folder any time.
- Your API key is stored in plain text in `%APPDATA%\Scribetray\config.toml`. A restricted key limits what it can do if it ever leaks.
- Scribetray sends transcription requests to the selected provider. It checks monthly ElevenLabs usage only while ElevenLabs is selected and your key permits it. A manual retry from history uses the provider currently selected in settings.
- The OpenAI provider reads your Codex access token locally and sends it to ChatGPT. It does not store another copy or use the refresh token.

## Settings

Most options are in the tray menu. Everything else lives in `%APPDATA%\Scribetray\config.toml`, which reloads automatically when you save it:

Changes saved during a recording apply after it stops. The recording uses the
provider selected when it started.

```toml
provider = "elevenlabs"    # or "openai": use the local Codex ChatGPT session
api_key = ""               # or set ELEVENLABS_API_KEY
hotkey = "Win+Alt+V"       # letters, digits, F1–F24, Space, Tab… with Win/Alt/Ctrl/Shift
mode = "toggle"            # or "push_to_talk": hold the hotkey while you speak
model = "scribe_v2"
language = "auto"          # or a language code such as "ita" or "eng"
prefix = "🎙️ "
prefix_enabled = true
auto_enter = false         # also press Enter after every dictation
intercept_enter = true     # plain Enter stops recording; Shift+Enter passes through
insert_method = "type"     # or "paste"
restore_clipboard = true
max_seconds = 600          # a recording stops by itself after 10 minutes
scribe_credits_per_hour = 585 # estimated batch Scribe credits per recording hour
microphone = ""            # blank uses the Windows default input device
realtime = false           # ElevenLabs only; preserved while OpenAI is selected
keyterms = []              # words Scribe should expect, e.g. ["Scribetray", "Kubernetes"]
sound_cues = true
start_with_windows = false
```

## Good to know

- **Windows only**, by design: it relies on Windows APIs for hotkeys, caret tracking, and typing. It has been tested mostly on Windows 11.
- **Apps running as administrator** can't receive typed text from a normal app (a Windows security rule). Run Scribetray as administrator too if you need that.
- **The waveform follows your text cursor** in most apps. Where an app doesn't expose its cursor, it appears next to the mouse pointer instead.
- **Terminals and games** may not handle typed Unicode text well. Paste mode can help.

## Build from source

You need Rust 1.98+ with the MSVC toolchain, CMake, NASM and libclang.
The native tools compile the OpenAI client's TLS library. They are build-time
requirements; users downloading the EXE do not need to install them. See
[native tool setup](docs/development.md#native-build-tools) before building.

```powershell
git clone https://github.com/colthreepv/scribetray
cd scribetray
. .\scripts\native-tools.ps1
cargo build --release --locked
.\target\release\scribetray.exe
```

To run your own builds day to day, `scripts/deploy-latest.ps1` builds, keeps a few versioned copies, and points a stable `latest` path at the newest one. See [docs/development.md](docs/development.md).

## License

Scribetray is distributed under the [Do What The Fuck You Want To Public License, Version 2 (WTFPL)](LICENSE). It is an independent project and is not affiliated with or endorsed by ElevenLabs or OpenAI.

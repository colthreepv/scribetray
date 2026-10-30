<p align="center">
  <img src="docs/media/banner.png" alt="Scribetray: speak into any text field on Windows" width="100%">
</p>

<p align="center">
  <b>Press a hotkey, talk, and your words appear where your cursor is.</b><br>
  A tiny Windows tray app that transcribes with ElevenLabs Scribe, or with OpenAI through your Codex ChatGPT login.
</p>

<p align="center">
  <img alt="Windows 10/11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?logo=windows&logoColor=white">
  <img alt="Written in Rust" src="https://img.shields.io/badge/Rust-1.98%2B-B7410E?logo=rust&logoColor=white">
  <img alt="License: WTFPL" src="https://img.shields.io/badge/license-WTFPL-2EA44F">
</p>

---

1. Put your cursor in a text field.
2. Press **Win+Alt+V** and speak. A small waveform floats next to your cursor so you know it's listening.
3. Press **Win+Alt+V** again. A moment later the text is typed in, right where you left it.

Press **Enter** instead to insert the text *and send it*, which is handy in chat boxes and AI prompts. Press **Esc**, the default cancel shortcut, to throw the recording away.

<p align="center">
  <img src="docs/media/overlay-recording.gif" alt="The recording overlay: a small pill with a live waveform" width="330">
</p>

It works in any app you can type into, and ships as a single executable: no installer, no admin rights, no telemetry.

## Get started

Pick the provider that matches the account you already have. You can switch later by changing `provider` in the settings file. The tray icon appears faded until the selected provider has usable credentials.

| Provider | Sign-in | Transcription |
|---|---|---|
| **ElevenLabs Scribe** | ElevenLabs API key | After recording, or live while you speak |
| **OpenAI** (experimental) | ChatGPT login saved by Codex app or CLI | After recording |

OpenAI support uses the private dictation endpoint of the ChatGPT desktop app, so it may stop working if OpenAI changes it.

### 1. Download and run

Download `scribetray.exe` from the [latest GitHub release](https://github.com/colthreepv/scribetray/releases/latest) and put it somewhere permanent, for example `%LOCALAPPDATA%\Programs\Scribetray\`. Then run it.

> [!NOTE]
> The executable isn't code-signed yet, so Windows SmartScreen may warn you the first time. Choose **More info → Run anyway**. You can also [build it yourself](#build-from-source).

### 2. Connect a provider

#### ElevenLabs Scribe

Scribetray uses your own ElevenLabs account, so there is no subscription to Scribetray itself.

1. [Sign up at ElevenLabs](https://elevenlabs.io/app/sign-up). The free plan includes 10,000 credits a month, enough to try Scribetray properly.
2. Open **Developers → API Keys → Create API key**.
3. Turn on **Restrict key** and enable only:
   - **Speech to Text**, required.
   - **User → Read**, optional. It lets the tray menu show how much of your monthly allowance you've used.
4. Right-click the tray icon, choose **Set API key…**, and paste the key into `api_key = "..."`. Save the file; Scribetray picks up the change immediately.

Alternatively, set the `ELEVENLABS_API_KEY` environment variable.

#### OpenAI through Codex

1. Sign in to Codex app or Codex CLI with your ChatGPT account on this computer.
2. Right-click the tray icon, choose **Open settings file…**, and set `provider = "openai"`.
3. Save the file. The menu shows the detected account; no API key is needed.

Scribetray needs the ChatGPT login that Codex saves in `%USERPROFILE%\.codex\auth.json`, or in `%CODEX_HOME%\auth.json` when that variable is set. API-key logins and sessions stored only in Windows Credential Manager are not supported. Scribetray only reads the file: if the session expires, sign in again through Codex and Scribetray picks up the new login without a restart.

## Everyday use

| Default key | While idle | While recording |
|---|---|---|
| **Win+Alt+V** | Start recording | Stop and insert |
| **Enter** | (untouched) | Stop, insert, and send, when Enter interception is enabled |
| **Shift+Enter** | (untouched) | Passed to the app (usually a line break); keep recording |
| **Esc** | (untouched) | Cancel the recording |

Change the recording hotkey from the tray menu. `hotkey` and `cancel_hotkey` can also be set in the [settings file](#settings).

**The tray icon** shows what's happening: a coral wave when ready, blue while recording, amber while transcribing, and a red × if something failed.

<p align="center">
  <img src="docs/media/tray-states.png" alt="Tray icon states on dark and light taskbars" width="520">
</p>

- **Left-click** the icon to copy your last dictation, or to retry it if it failed.
- **Right-click** for the menu. From there you can choose the microphone and language, browse recent dictations, switch to push-to-talk, and change the recording hotkey.

Turn off **Intercept Enter while recording** in the menu if you want Enter to reach the app while you dictate. The cancel shortcut and Enter are claimed only while a recording is running.

If the field lost focus before the text was ready, Scribetray copies the text to the clipboard and waits: click back into the same field and it is typed in after a moment. After 2.5 minutes, or when you start a new recording, it stays only on the clipboard. Every dictation starts with 🎙️ by default, so readers know it was spoken; turn the prefix off in the menu.

With ElevenLabs, the menu also shows your monthly credit usage and an estimate of the batch transcription time left. With OpenAI, it shows the signed-in account.

## What does it cost?

Scribetray is free and open source. ElevenLabs Scribe is paid per audio duration at its current [pricing](https://elevenlabs.io/pricing); the free plan includes 10,000 credits a month, and `trim_silence` shortens long pauses before they are billed. OpenAI uses your ChatGPT login; Scribetray cannot see any dictation quota, so it shows none.

## Privacy

- Audio goes only to the provider you selected: when you stop recording, or while you speak with ElevenLabs realtime. Its privacy and retention policies apply.
- Your last 20 recordings (audio and text) are kept on your PC in `%LOCALAPPDATA%\Scribetray\history`, so that nothing is lost when something fails. Delete that folder any time.
- Your API key is stored in plain text in `%APPDATA%\Scribetray\config.toml`. A restricted key limits what it can do if it ever leaks.
- With OpenAI, Scribetray reads the Codex access token locally and sends it only to ChatGPT. It does not save its own copy of the token or use the refresh token.

## Settings

Most options are in the tray menu. Everything else lives in `%APPDATA%\Scribetray\config.toml`, which reloads when you save it. Changes saved during a recording apply to the next one.

```toml
provider = "elevenlabs"    # or "openai": use the local Codex ChatGPT session
api_key = ""               # or set ELEVENLABS_API_KEY
hotkey = "Win+Alt+V"       # letters, digits, F1–F24, Space, Tab… with Win/Alt/Ctrl/Shift
cancel_hotkey = "Esc"      # cancels the active recording; must differ from hotkey
mode = "toggle"            # or "push_to_talk": hold the hotkey while you speak
model = "scribe_v2"        # ElevenLabs only
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
realtime = false           # ElevenLabs only; kept but ignored with OpenAI
trim_silence = true        # shorten long pauses before batch upload
keyterms = []              # words Scribe should expect, e.g. ["Scribetray", "Kubernetes"]
sound_cues = true
start_with_windows = false
```

A few options deserve a word:

- **`cancel_hotkey`** defaults to `Esc` and is active only while recording. Choose another shortcut, such as `Win+Alt+X`, if you need Esc in the app you are dictating into.
- **`trim_silence`** shortens pauses longer than 0.7 seconds on your PC before upload, with either provider. Live (realtime) transcription is unaffected, and recordings too short or too noisy to judge are sent unchanged.
- **`realtime`** works with ElevenLabs only. OpenAI ignores it, and the value is kept for when you switch back.

## Good to know

- **Windows only**, by design: it relies on Windows APIs for hotkeys, caret tracking, and typing. It has been tested mostly on Windows 11.
- **Apps running as administrator** can't receive typed text from a normal app (a Windows security rule). Run Scribetray as administrator too if you need that.
- **The waveform follows your text cursor** in most apps. Where an app doesn't expose its cursor, it appears next to the mouse pointer instead.
- **Terminals and games** may not handle typed Unicode text well. Paste mode can help.

## Build from source

You need Rust 1.98+ with the MSVC toolchain, plus CMake, NASM and libclang to compile the TLS library of the OpenAI client. They are needed only to build; the downloaded EXE does not need them. See [native tool setup](docs/development.md#native-build-tools).

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

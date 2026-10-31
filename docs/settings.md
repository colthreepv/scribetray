# Settings

Open the settings file from the tray menu, or edit `%APPDATA%\Scribetray\config.toml`. Scribetray reloads it when you save. A recording keeps the settings it started with; recording changes apply to the next one. Missing options use their defaults.

For account setup and everyday use, see the [README](../README.md).

## Provider and transcription

| Option | Default | What it does |
|---|---|---|
| `provider` | `"elevenlabs"` | Selects `"elevenlabs"` or `"openai"`. |
| `api_key` | `""` | ElevenLabs API key. A nonblank `ELEVENLABS_API_KEY` environment variable takes precedence. OpenAI does not use this setting. |
| `realtime` | `false` | Enables ElevenLabs audio streaming. OpenAI always streams and ignores this value, which is kept if you switch back. Both providers insert final text only after recording stops. |
| `trim_silence` | `true` | Shortens long pauses before batch uploads, including a retry after streaming fails. Does not alter streaming audio or the original recording in history. |
| `model` | `"scribe_v2"` | ElevenLabs batch model. ElevenLabs streaming uses `scribe_v2_realtime`; OpenAI ignores this setting. |
| `language` | `"auto"` | Detects the language automatically, or sends the chosen code to the provider. The tray menu offers language choices. `language_code` is also accepted as a legacy name. |
| `keyterms` | `[]` | Vocabulary hints for ElevenLabs streaming, such as `["Scribetray", "Kubernetes"]`. Currently unused by batch transcription and OpenAI. Accepts up to 50 nonempty terms, each at most 20 characters after trimming surrounding whitespace. |
| `scribe_credits_per_hour` | `585` | Estimate used to show remaining ElevenLabs batch transcription time. Adjust it for your account; it does not set the provider's rate. |

Silence trimming looks for pauses of at least 0.7 seconds. Audio too short or too noisy to judge stays unchanged. If a stream fails, Scribetray makes one batch retry with the same provider after you stop. The retry uses this trimming setting.

OpenAI authentication comes from `%CODEX_HOME%\auth.json` when `CODEX_HOME` is set, otherwise `%USERPROFILE%\.codex\auth.json`. The file must contain a ChatGPT session; API-key logins and keychain-only credentials are unsupported. Scribetray reads the file without changing it or renewing tokens. Sign in again through Codex if the session expires.

## Recording and shortcuts

| Option | Default | What it does |
|---|---|---|
| `hotkey` | `"Win+Alt+V"` | Starts and stops recording in toggle mode. Supports Win, Alt, Ctrl, and Shift modifiers with letters, digits, F1 through F24, and keys such as Space or Tab. |
| `cancel_hotkey` | `"Esc"` | Cancels the active recording. Only claimed while recording; must differ from `hotkey`. |
| `mode` | `"toggle"` | Press the hotkey to start and again to stop. Use `"push_to_talk"` to record while holding it. |
| `microphone` | `""` | Uses the Windows default input device. Set a device name or choose one from the tray menu. |
| `max_seconds` | `600` | Stops recording at this duration, in seconds. Must be greater than zero. |
| `intercept_enter` | `true` | Plain Enter stops recording, inserts the transcript, and sends Enter to the app. Shift+Enter always passes through. Set to `false` to let plain Enter reach the app during recording. |

Cancelling a streaming recording cannot retract audio already sent to the provider.

## Text insertion

| Option | Default | What it does |
|---|---|---|
| `prefix` | `"🎙️ "` | Text added before each inserted transcript when enabled. |
| `prefix_enabled` | `true` | Includes the prefix. |
| `auto_enter` | `false` | Sends Enter after every dictation, including when stopped with the hotkey. |
| `insert_method` | `"type"` | Simulates typing. Use `"paste"` to insert through the clipboard instead. |
| `restore_clipboard` | `true` | Restores previous clipboard contents after paste insertion. |

If the original field loses focus, Scribetray copies the transcript to the clipboard and waits up to 20 seconds for focus to return. Starting another recording ends that wait. This timeout is fixed, not a setting.

## Sounds and startup

| Option | Default | What it does |
|---|---|---|
| `sounds` | `"important"` | `"all"` plays every cue; `"important"` plays errors and pending insertion; `"off"` is silent. |
| `start_with_windows` | `false` | Starts Scribetray when you sign in to Windows. |

History keeps the latest 20 recordings in `%LOCALAPPDATA%\Scribetray\history`. The history limit is fixed, not a setting.

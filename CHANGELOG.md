# Changelog

## v0.6.3

Removes unused internal helpers; no user-visible behavior changes.

## v0.6.2

Preserves recordings through recoverable audio-capture glitches, logs user
notices even when Windows suppresses notification balloons, and fixes the
hotkey-capture dialog's message loop. Adds square social artwork and its source
renderer.

## v0.6.1

Added a first-run API-key reminder and comments to the generated configuration
file so new users can set up ElevenLabs without hunting through the source.

## v0.6.0

Replaced the plain ElevenLabs usage line with a native tray-menu progress header
and an approximate remaining batch-time estimate controlled by
`scribe_credits_per_hour`.

## v0.5.1

Added per-user versioned builds and a stable `latest` deployment path.

## v0.5.0

Added cached ElevenLabs account usage to the tray menu.

## v0.4.0

Added recoverable recording history, retry, clipboard fallback, and recording
controls for inserting or submitting text.

## v0.3.0

Added the Scribe v2 Realtime baseline and refreshed the tray and recording
overlay UI.

## Earlier development

The untagged commits introduced the Windows tray app, global hotkey, batch
transcription, local history, language selection, and text insertion.

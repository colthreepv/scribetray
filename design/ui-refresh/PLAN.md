# Scribetray — open work (v0.5)

The v0.3 UI refresh and the v0.4 recovery and send plan have shipped (`82a9f7a`, `9efb2e8`). This file lists only what is still open. Implement it as written, in this order.

## 1. ElevenLabs usage line in the tray menu

Show a single grayed, non-clickable line at the top of the right-click menu, above "Start recording":

```
ElevenLabs: 1,084 / 23,130 credits (5%) · resets Oct 6
```

- Source: `GET https://api.elevenlabs.io/v1/user/subscription` with the existing `api_key` (`xi-api-key` header). Use `character_count`, `character_limit`, and `next_character_count_reset_unix`. Format the reset date with the OS locale (`GetDateFormatEx`, month + day). If `current_overage.amount` is non-zero, append `· overage $X`. Omit the reset part when the timestamp is `null`.
- Refresh: once at startup, then in the background when the menu opens and the cached value is older than 10 minutes. The menu always renders immediately from the cache and never waits for the network. Do not fetch after each transcription.
- Permissions: the key must have `user_read` in addition to `speech_to_text`. If the call returns 401 or 403, hide the line, log one warning naming the missing permission, and don't retry until the config is reloaded. On other errors, keep showing the last good value; if there is none, hide the line.
- No owner-drawn progress bar for now.

Accept: with the current key, the menu shows the line above. With a key lacking `user_read`, the line is absent and nothing else changes.

## 2. Overlay countdown (low priority)

When 60 s or less remain before `max_seconds`, widen the pill to 90 DIP and show an amber (`#F5A524`) `0:42` countdown right-aligned. Use 11 px Segoe UI Variable Semibold (fall back to Segoe UI), 10 DIP from the right edge. The reference drawing is `overlay-prototype.html` ("Recording, 0:42 left").

Accept: a recording running near the limit shows the countdown in its last minute. Recordings that stay under the limit look unchanged.

## 3. Push-to-talk dot (lowest priority)

In push-to-talk mode, draw the overlay's recording dot as a hollow red ring with a 1.5 DIP stroke instead of a solid dot, so it is clear that releasing the key stops recording. Push-to-talk is lightly used, so this goes last.


# Scribetray — recovery and send plan (v0.4)

This replaces the v0.3 UI refresh plan, which has shipped in `82a9f7a`. Three small leftovers from that plan are listed at the end. Everything here was decided with the user; implement it as written.

## 1. Tray icon: caret + voice (assets done)

The tray icon is now a text caret in the taskbar color with a three-bar voice wave. The wave color carries the state, so there is no badge. The icon deliberately contains no microphone, so it doesn't compete with the Windows privacy mic indicator.

| State | Wave |
|---|---|
| idle | coral `#F0564A` |
| recording | blue `#3B8BFF` |
| working | amber `#F5A524` |
| error | red × replaces the wave |
| off | whole glyph at 45% opacity |

The icons are already regenerated in `design/ui-refresh/out/tray/` with the same file names and resource IDs, and `build.rs` embeds them from there. **Implementation: rebuild only.** The error rule is unchanged: it clears on the next successful dictation or when History is opened, whichever comes first. The tray icon is a nice-to-have; do not spend effort beyond this.

Preview: `out/tray-states-preview.png`. Generator: `tools/render_assets.py` (`tray_icon`).

## 2. History as a recovery list

History exists to recover dictations that did not reach their field. The menu should answer one question: *which one do I need?*

Row format: the transcript preview, then `\t`, then the duration as `MMmSSs`. The `\t` right-aligns the duration in the shortcut column. Remove the date completely, including `history_timestamp_label`; list order already means "newest first".

```
Ciao, potresti effettuare una revisione…      01m41s
Sure, my dear. How are you?                   00m03s
⚠ Allora, ti do un po' di feedback…           01m44s
✕ Transcription failed — click to retry       00m08s
Transcribing…                                 00m12s   (grayed)
```

- **⚠ = not delivered.** The transcript exists but was only copied to the clipboard: insertion failed, the focus guard rejected the target, or delivery was `Delivery::Clipboard`. Clicking the row copies it, the same as unmarked rows.
- **✕ = transcription failed.** Clicking retries (existing `HistoryRetry`).
- **Hidden:** succeeded recordings with an empty or whitespace transcript (today they show as "No transcript"). They still count toward the 20 stored on disk, and the menu shows the 10 newest visible rows.
- **Pending** rows are grayed with no action.
- Duration comes from `duration_seconds`. The cap is `max_seconds` = 600, so `MMmSSs` always fits. Format with `format!("{:02}m{:02}s", s / 60, s % 60)`.

Data change in `history.rs`: add `delivered: Option<bool>` to `Recording` with `#[serde(default, skip_serializing_if = "Option::is_none")]`. `None` (older records, or not yet delivered) shows no marker. Add `History::mark_delivered(id, bool)`. In `main.rs`, call it with `true` after a successful insertion. Call it with `false` in both clipboard fallbacks: the `Err` branch of paste/type delivery, and `Delivery::Clipboard | None`.

Accept: a dictation whose target window lost focus shows ⚠ in History, while a normal one shows no marker. Silence-only recordings don't appear, and no date appears anywhere in the menu.

## 3. Left click = recover the last dictation

Left click (`WM_LBUTTONUP` / `NIN_SELECT`, which currently open the menu) acts on the **newest visible history row**, using the same visibility rule as §2:

| Newest row | Action | Balloon |
|---|---|---|
| has a transcript | copy to clipboard | `Copied: "Ciao, potresti effettuare…" · 01m41s` |
| failed | retry transcription | `Retrying 00m08s…` (the normal completion flow then delivers to clipboard) |
| pending | nothing | `Still transcribing…` |
| no history | nothing | `Nothing to recover yet` |

Right click keeps the full menu. Copying from a left click counts as "History opened" for the error rule in §1. Extend the idle tooltip to `Scribetray — Ready (Win+Alt+V) · click: copy last dictation`.

Why left click doesn't start recording: clicking the tray makes the taskbar the foreground window, so `capture_target` would snapshot the taskbar rather than the text field.

Accept: after a dictation lands in the wrong place, one left click puts it on the clipboard with a confirming balloon.

## 4. Enter stops and sends; remove the submit hotkey

The key that ends a recording decides what happens:

| While recording | Result |
|---|---|
| Win+Alt+V (toggle hotkey) | stop, insert (plus Enter only if the Auto-Enter setting is on) |
| **Enter** | stop, insert, then press Enter once (same as today's `submit_on_complete = true`) |
| Esc | cancel |

- Register `VK_RETURN` with no modifiers in `set_recording(true)`, next to the existing Escape registration, and unregister it in the same places Esc is unregistered. A registration failure only logs a warning; recording continues without Enter-to-send.
- The Enter hotkey maps to the existing submit path in `main.rs` (`submit_after_transcription`), so realtime and batch behave the same. In push-to-talk mode Enter works too: pressing it while holding the combo stops and sends.
- Before inserting the text and before the synthetic Enter, wait until the physical Enter key is released (`GetAsyncKeyState(VK_RETURN)`). This goes next to the existing wait for Win/Alt/Shift release, and it prevents a double send.
- **Remove** the submit hotkey entirely: `hotkey_submit` in `config.rs` and its default `Win+Alt+Shift+V`, `HotkeyPurpose::SubmitRecording` and its registration and collision check, `UiSettings::submit_hotkey`, and both "Record and send" menu items. Make sure old config files that still contain `hotkey_submit` load without error (serde ignores unknown keys unless `deny_unknown_fields` is set).
- No menu entry for this. It is documented behavior: README, plus the recording tooltip `Recording 00:35 · Enter to send · Esc to cancel`.

Accept: in the Codex composer, Win+Alt+V, speaking, then Enter inserts the text and submits once. An Enter pressed while idle is never intercepted.

## 5. Leftovers from v0.3 (low priority)

1. Overlay countdown: when ≤ 60 s remain before `max_seconds`, widen the pill to 90 DIP and show an amber `0:42`. The prototype has the reference drawing (`overlay-prototype.html`, "Recording, 0:42 left").
2. Push-to-talk: a hollow red ring instead of the solid dot in the overlay. Push-to-talk is lightly used, so this goes last.

Dropped: the `overlay_timer` / `overlay_position` config keys. The fixed defaults are fine.

## Order

§2 data change → §3 left click → §4 Enter/remove submit hotkey → rebuild for §1 → §5 if time allows. Update README for §3 and §4.


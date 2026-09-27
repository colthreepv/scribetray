# M0 validation matrix

Status recorded 2026-09-28. This matrix captures the manual desktop checks
available so far; `cargo check` and a Windows release build also pass.

| Application | Caret anchor | Hotkey and paste | Realtime Scribe | Auto-Enter | Status |
|---|---|---|---|---|---|
| Codex App | User confirmed the anchor appears at the dictation target. Runtime logs from the initial run reported the mouse-position fallback, so exact caret detection is unverified. | User confirmed a transcript was inserted into the composer. | User dictated this conversation with Realtime enabled; transcription and insertion succeeded. | User reported an error after enabling it. The Enter focus guard has been adjusted, but needs a retest against the updated build. | Partial pass |
| Chrome | — | — | — | — | Not tested |
| VS Code | — | — | — | — | Not tested |
| Notepad | — | — | — | — | Not tested |
| Windows Terminal | — | — | — | — | Not tested |

The Win+Alt+V and Win+Alt+Shift+V registrations were confirmed in the app log.
Push-to-talk still needs a desktop check. The Codex App anchor and realtime
round-trip are user-confirmed; this does not establish that UI Automation found
the actual caret because the observed method was the mouse fallback.

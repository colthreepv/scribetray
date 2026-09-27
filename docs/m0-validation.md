# M0 validation matrix

Status recorded 2026-09-28. This matrix captures the manual desktop checks
available so far; `cargo check` and a Windows release build also pass.

| Application | Caret anchor | Hotkey and paste | Realtime Scribe | Auto-Enter | Status |
|---|---|---|---|---|---|
| Codex App | User confirmed the anchor appears at the dictation target. Earlier logs used the mouse fallback; the updated build has now reported `UiaTextPattern`. | User confirmed a transcript was inserted into the composer. | User confirmed transcription and insertion, then reported that long dictations retained only the latest segment. The app now commits and joins segments; a long-run retest is pending. | User reported an error after enabling it. The Enter focus guard has been adjusted, but needs a retest against the updated build. | Partial pass |
| Chrome | — | — | — | — | Not tested |
| VS Code | — | — | — | — | Not tested |
| Notepad | — | — | — | — | Not tested |
| Windows Terminal | — | — | — | — | Not tested |

The Win+Alt+V and Win+Alt+Shift+V registrations were confirmed in the app log.
Push-to-talk still needs a desktop check, along with repeatability of UIA caret
detection and the full application matrix.

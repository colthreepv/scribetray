# M0 validation matrix

Status recorded 2026-09-28. This matrix captures the manual desktop checks
available so far; `cargo check` and a Windows release build also pass.

| Application | Caret anchor | Hotkey and paste | Realtime Scribe | Auto-Enter | Status |
|---|---|---|---|---|---|
| Codex App | User confirmed the anchor appears at the dictation target. Earlier logs used the mouse fallback; updated runs have reported both `UiaTextPattern` and `MouseFallback`. | User confirmed a transcript was inserted into the composer. | User reported that long dictations retained only the latest segment. The updated app commits and joins segments; user then confirmed a 46-second Realtime recording retained the full message. Local history records it as succeeded with 663 transcript characters. A two-commit WebSocket mock also passes. | User reported an error after enabling it. The Enter focus guard has been adjusted, but needs a retest against the updated build. | Pass for this scenario |
| Chrome | — | — | — | — | Not tested |
| VS Code | — | — | — | — | Not tested |
| Notepad | — | — | — | — | Not tested |
| Windows Terminal | — | — | — | — | Not tested |

The Win+Alt+V and Win+Alt+Shift+V registrations were confirmed in the app log.
The local WebSocket mock test and the 46-second Realtime run pass. The user
also tried Type mode with batch transcription and reported that insertion was
slow; this is expected because Type mode sends Unicode keystrokes instead of a
single paste operation, and is mainly useful for fields that block paste. The
user enabled Auto-Enter in a recent run, but its submission result was not
reported. Auto-Enter and push-to-talk still need clear desktop checks, along
with the full application matrix.

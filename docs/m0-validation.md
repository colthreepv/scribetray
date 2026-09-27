# M0 validation matrix

Status recorded 2026-09-28. This matrix captures the manual desktop checks
available so far; `cargo check` and a Windows release build also pass.

| Application | Caret anchor | Hotkey and paste | Realtime Scribe | Auto-Enter | Status |
|---|---|---|---|---|---|
| Codex App | User confirmed the anchor appears at the dictation target. Earlier logs used the mouse fallback; updated runs have reported both `UiaTextPattern` and `MouseFallback`. | User confirmed a transcript was inserted into the composer. Type mode is currently selected in the installed config and the user reported no noticeable speed penalty. | User reported that long dictations retained only the latest segment. The updated app commits and joins segments; user then confirmed a 46-second Realtime recording retained the full message. Local history records it as succeeded with 663 transcript characters. A two-commit WebSocket mock also passes. | User confirmed Auto-Enter works. An earlier attempt reported an error despite successful transcription/insertion; the user suspects the successful tests may be related to Type mode, but the causal link is unverified. | Pass for current tested flow |
| Chrome | — | — | — | — | Not tested |
| VS Code | — | — | — | — | Not tested |
| Notepad | — | — | — | — | Not tested |
| Windows Terminal | — | — | — | — | Not tested |

The Win+Alt+V and Win+Alt+Shift+V registrations were confirmed in the app log.
The local WebSocket mock test and the 46-second Realtime run pass. The user
also tried Type mode with batch transcription and reported no obvious speed
penalty. Type mode sends Unicode keystrokes instead of a single paste operation
and is mainly useful for fields that block paste; new configurations now
default to Type mode. The user confirmed Auto-Enter works. An earlier attempt
reported an error despite successful transcription/insertion; whether this
relates to the insert method is unverified. Push-to-talk and the broader
application matrix still need desktop checks. Windows Terminal is a secondary
compatibility target, not a primary dictation workflow; Codex App remains the
main validation target.

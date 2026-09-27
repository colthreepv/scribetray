# M0 validation matrix

Status recorded 2026-09-28. This matrix captures the manual desktop checks
available so far; `cargo check` and a Windows release build also pass.

| Application | Caret anchor | Hotkey and paste | Realtime Scribe | Auto-Enter | Status |
|---|---|---|---|---|---|
| Codex App | User confirmed the anchor appears at the dictation target. Earlier logs used the mouse fallback; updated runs have reported both `UiaTextPattern` and `MouseFallback`. | User confirmed a transcript was inserted into the composer. Type mode is selected and has no noticeable speed penalty. | User confirmed a 46-second Realtime recording retained the full message. A two-commit WebSocket mock also passes. | User confirms Auto-Enter works in Type mode. | Pass for current tested flow |
| Chrome | — | User reports Auto-Enter does not submit from the search field in Paste mode; Type mode is preferred. Caret detection method was not recorded. | — | Paste-mode submission failure reported. | Partial user report |
| VS Code | — | — | — | — | Not tested |
| Notepad | — | — | — | — | Not tested |
| Windows Terminal | — | — | — | — | Not tested |

Caret discovery now also asks the focused child window for an MSAA caret when
Windows reports no dedicated caret window. This code path still needs a desktop
check in Codex App and other Chromium/Electron controls.

The user reports no audible sound cues despite the option being enabled, and
that Paste mode can leave the previous clipboard replaced. Sound playback and
clipboard restoration have been changed and need another desktop check. Paste
restoration errors no longer suppress Auto-Enter after a successful paste.

The Win+Alt+V and Win+Alt+Shift+V registrations were confirmed in the app log.
The local WebSocket mock test and the 46-second Realtime run pass. The user
also tried Type mode with batch transcription and reported no obvious speed
penalty. Type mode sends Unicode keystrokes instead of a single paste operation
and is mainly useful for fields that block paste; new configurations now
default to Type mode. The user confirmed Auto-Enter works in Type mode and
reported that it fails with Paste mode in Chrome's search field. Push-to-talk
and the broader application matrix still need desktop checks. Windows Terminal is a secondary
compatibility target, not a primary dictation workflow; Codex App remains the
main validation target.

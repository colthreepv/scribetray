# Scribetray UI refresh — plan and assets

This folder is a design hand-off. It contains no application code changes. An implementer should be able to build it from this file, the generated assets in \`out/\`, and the reference drawing code in \`overlay-prototype.html\`.

| File | Purpose |
|---|---|
| \`tools/render_assets.py\` | Reproducible generator: \`uv run design/ui-refresh/tools/render_assets.py\` |
| \`out/scribetray.ico\` | New app icon (16–256 px), replaces \`assets/scribetray.ico\` |
| \`out/tray/tray-{idle,recording,working,error,off}-{dark,light}.ico\` | Tray icons, 16/20/24/32/40/48 px each |
| \`out/tray-states-preview.png\` | Contact sheet of all tray states on dark and light taskbars |
| \`out/overlay-recording.gif\`, \`out/overlay-working.gif\`, \`out/overlay-states.png\` | Overlay previews (3× scale) |
| \`overlay-prototype.html\` | Live reference: states, placement, DPI scale, timer modes, real microphone input |

## 1. Review of the current UI

1. **Tray icon never changes.** \`create_ui_state\` loads resource 1 once and \`Shell_NotifyIconW(NIM_MODIFY)\` is only used for balloons. The icon has a red dot above a white stand, so it reads as a traffic light that is always red. It carries no state.
2. **Only one icon size.** \`assets/scribetray.ico\` contains a single 64×64 image. \`LoadIconW\` returns the 32 px system size, and the shell then scales it to 16/20/24 px, so it looks blurry at every DPI.
3. **The tray tooltip is invisible.** The code sets \`NOTIFYICON_VERSION_4\` without \`NIF_SHOWTIP\`. With version 4 the shell shows the standard tooltip only when that flag is set.
4. **Overlay is large, opaque, and ignores DPI.** The overlay is 152×44 *physical* px in a PerMonitorV2 process, drawn with GDI into an \`LWA_COLORKEY\` window. Color keying cannot antialias the rounded corners or fade. The system bitmap font is used for text, and the pill sits 10 px below the caret, where it covers UI such as "Full access" in the Codex composer.
5. **The recording overlay shows time, not signal.** A blinking dot and mm:ss cannot tell you whether the microphone hears you, which is the question you have while you talk. A live level meter answers it.
6. **The menu is flat and mixes kinds of items.** Eleven top-level entries mix actions, persistent preferences, and mutually exclusive modes, all with the same checkmark style. It also includes a grayed sentence explaining Type mode. Hotkeys are missing from the labels, and the default action (left-click) isn't marked in the menu.

## 2. Tray icon

The idle icon is a **monochrome microphone glyph**, like Windows system tray icons: white on a dark taskbar, near-black on a light one. State is shown only by a **badge** in the lower-right corner, with a 1.25 px transparent gap cut into the glyph. Nothing flashes.

| State | Badge | When |
|---|---|---|
| idle | none | Ready |
| recording | solid red dot \`#F04438\` | Capturing audio |
| working | solid amber dot \`#F5A524\` | Upload / waiting for Scribe |
| error | red dot with white × | Last attempt failed; clears on the next recording or when the menu opens |
| off | glyph with a slash | No API key, no input device, or hotkey registration failed |

Implementation notes:

- Embed the 10 icons as resources (\`winresource\` supports \`set_icon_with_id\`) or \`include_bytes!\` them and use \`CreateIconFromResourceEx\`. Load each with \`LoadIconMetric(hinst, id, LIM_SMALL)\`, which picks the right size for the current DPI. Reload on \`WM_DPICHANGED\`/\`WM_SETTINGCHANGE\`.
- Pick the theme from \`HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\\SystemUsesLightTheme\` (the taskbar value, which differs from \`AppsUseLightTheme\`). Re-read it on \`WM_SETTINGCHANGE\` with \`lParam == "ImmersiveColorSet"\`.
- Swap icons with \`NIM_MODIFY\` + \`NIF_ICON\` whenever the \`AnchorStatus\`-equivalent app state changes.
- Tooltip: add \`NIF_SHOWTIP\` and update \`szTip\` with the state:
  - \`Scribetray — Ready (Win+Alt+V)\`
  - \`Scribetray — Recording 00:35 · Esc to cancel\` (update once per second while recording)
  - \`Scribetray — Transcribing…\`
  - \`Scribetray — Last dictation failed · click History to retry\`
  - \`Scribetray — Set an API key in settings\`

The **app icon** (\`out/scribetray.ico\`) is a separate, colored brand tile: a warm coral-to-crimson gradient with a white mic. At 48 px and above, two waveform bars appear on each side. It is used for the exe, the Start menu, and Settings. It should never appear in the tray.

## 3. Caret overlay: a small live waveform

Replace the 152×44 card with a **58×22 DIP pill**, roughly the height of a line of text. \`overlay-prototype.html\` is the reference implementation; port its \`frame()\` function directly.

Geometry (DIPs, multiply by the scale of the caret's monitor):

- Pill 58×22, radius 11, fill \`rgba(22,24,29,0.94)\`, 1 px inner border \`rgba(255,255,255,0.13)\`. Soft shadow of 8 px blur, 2 px y-offset at 35% black, so the window is the pill plus 8 px padding on each side.
- Status dot at x = 11, r = 3. When recording it is red and breathes between 55% and 100% opacity over a 1.6 s period.
- Waveform: 7 bars, 2 px wide, 4 px pitch, starting at x = 21, with round caps and color \`#F0F2F5\`. Bar height = 2 + level × 12 (min 2, max 14), centered vertically.
- Bars scroll right to left: every 50 ms the newest level is pushed on the right. On screen each bar eases toward its target with \`shown += (target - shown) * 0.45\` per 33 ms frame. The bars look alive only while you actually speak; flat dots mean silence or a dead mic, which is useful feedback.

States:

| State | Visual |
|---|---|
| recording | red breathing dot + scrolling waveform |
| recording, ≤ 60 s before \`max_seconds\` | pill widens to 90 DIP; amber countdown \`0:42\` right-aligned, 11 px Segoe UI Variable Semibold |
| working | amber dot + three dots in a travelling wave (0.9 s period, 3 px amplitude) |
| done | collapses to a 22×22 circle with a green check; holds 0.7 s, fades 0.2 s |
| error | 22×22 circle with a red ×; holds 2 s, fades 0.2 s; details stay in the balloon and History |

Entry animation: 120 ms fade in, scaling from 0.92 to 1.

A new config key sets the timer mode: \`overlay_timer = "near_limit" | "always" | "never"\`, default \`near_limit\`. Elapsed time is always available in the tray tooltip.

Placement: add \`overlay_position = "above" | "right" | "below"\`, default \`above\`. For \`above\`, place the pill 8 DIP above the caret top, with its left edge 4 DIP left of the caret. Flip to below when there is no room, then clamp to the monitor's **work area** (\`MonitorFromRect\` + \`GetMonitorInfoW\`), not the virtual screen. The fallback chain is unchanged: when the caret cannot be found, use the mouse position.

Rendering:

- Replace \`LWA_COLORKEY\` + GDI with **\`UpdateLayeredWindow(ULW_ALPHA)\`** from a 32-bit premultiplied BGRA \`CreateDIBSection\`. Remove the \`SetLayeredWindowAttributes\` call; the two modes are mutually exclusive.
- Draw with **\`tiny-skia\`** (pure Rust, antialiased paths, premultiplied RGBA output; swap R/B when copying into the DIB). For the countdown digits, rasterize with \`ab_glyph\` from \`C:\\Windows\\Fonts\\SegUIVar.ttf\` (fall back to \`segoeui.ttf\`). Direct2D would also work but is more COM for little gain here.
- Animation timer: 33 ms \`SetTimer\` only while the overlay is visible. Kill it on hide. Keep the existing 250 ms caret follow.
- DPI: \`GetDpiForMonitor(MonitorFromRect(caret))\`, scale = dpi / 96. Re-rasterize when the scale changes.

Level plumbing from \`audio.rs\`:

- In the cpal input callback, accumulate the sum of squares of the mono f32 samples. About every 20 ms, compute \`db = 20·log10(rms + 1e-9)\`, \`target = clamp((db + 52) / 40, 0, 1)\`, and update an envelope: \`env += (target - env) * (target > env ? 0.6 : 0.15)\`.
- Store the envelope as \`f32::to_bits\` in an \`Arc<AtomicU32>\` owned by the recorder. Expose \`Recorder::level_meter() -> LevelMeter\` (a cheap clone of the Arc).
- \`main.rs\` passes the meter to the UI with a new \`UiCommand::SetLevelMeter(Option<LevelMeter>)\` when recording starts and \`None\` when it stops. The overlay timer samples it every 50 ms. There are no channels and no per-sample cross-thread traffic. The realtime and batch paths both use it.
- The -52 dBFS floor and 40 dB range match a typical headset. If quiet mics look flat, expose \`meter_floor_db\` in config rather than adding AGC.

## 4. Tray menu

Proposed structure (\`\\t\` right-aligns the shortcut text; **bold** = \`SetMenuDefaultItem\`, which matches left-click):

\`\`\`
**Start recording\tWin+Alt+V**          (Stop recording\tWin+Alt+V / Esc while recording)
Record and send\tWin+Alt+Shift+V
───────────────
Microphone                       ▸  ● System default / ○ device… (radio)
Language                         ▸  ● Auto / ○ Italiano / ○ English (radio)
History                          ▸  14:32  Ciao, potresti effettuare una re…   (click = copy)
                                     ⚠ 14:05  Failed — click to retry
                                     ─────
                                     Open history folder
───────────────
Recording                        ▸  ● Toggle (press again to stop) / ○ Push-to-talk (radio)
                                     ☑ Realtime transcription
                                     ☑ Sound cues
Insert                           ▸  ● Type keystrokes / ○ Paste via clipboard (radio)
                                     ☑ Add 🎙️ prefix
                                     ☐ Press Enter after inserting
───────────────
Change hotkey…
Open settings file
☑ Start with Windows
───────────────
Quit Scribetray
\`\`\`

Rationale and Win32 details:

- Mutually exclusive choices (Microphone, Language, Toggle/Push-to-talk, Type/Paste) get radio bullets via \`MFT_RADIOCHECK\` (\`InsertMenuItemW\` with \`MENUITEMINFOW\`) or \`CheckMenuRadioItem\`. Independent options keep checkmarks. This separates "pick one" from "turn on".
- Delete the grayed explanatory sentence. The label "Type keystrokes" says what it does; put the longer explanation in the README.
- Group the preferences you rarely change under **Recording** and **Insert**. The top level then holds what you use daily: actions, mic, language, history.
- History labels: \`HH:MM  first ~40 chars\`. Successful items are single click-to-copy entries. Failed items get a ⚠ prefix, and clicking one retries. This removes one submenu level per item. Keep at most 10 entries in the menu.
- When state is **off** (for example, no API key), the first item becomes **Set API key…** (opens the settings file) and recording is grayed.
- Keep \`TrackPopupMenu\` with \`TPM_RIGHTBUTTON\`, plus the existing \`SetForegroundWindow\` + \`PostMessage(WM_NULL)\` dance.
- Non-goal for now: a custom-drawn (dark-mode) menu. Windows 11 renders classic menus in dark mode only through the undocumented \`SetPreferredAppMode\` (uxtheme ordinal 135). Calling \`SetPreferredAppMode(AllowDark)\` before creating the tray window is a one-line experiment worth trying; leave it off if it misbehaves.

## 5. Work breakdown for the implementer

Each step is independently shippable, in this order:

1. **Icons + tooltip** (small). Swap in \`out/scribetray.ico\`, embed the tray ICOs, add \`LoadIconMetric\`, theme detection, and \`NIM_MODIFY\` on state change. Add \`NIF_SHOWTIP\` and the dynamic tooltip.
   *Accept:* crisp icons at 100/125/150/200%, the badge follows the state, hover shows the tooltip, and the light/dark taskbar switch updates live.
2. **Level meter** (small). Add \`LevelMeter\` in \`audio.rs\` and the \`UiCommand\` wiring. Unit-test only the dB→level mapping.
3. **Overlay renderer** (medium). Add tiny-skia + \`UpdateLayeredWindow\` and port \`frame()\` from the prototype. Add DPI scaling, the placement options, and the 33 ms timer lifecycle.
   *Accept:* it matches \`out/overlay-states.png\` at 100% and 200%, bars react to speech and stay flat in silence, CPU is under 1% while recording, and the overlay never takes focus.
4. **Menu restructure** (small–medium). Add submenus, radio items, shortcut text, the default item, flat history, and the off-state first item.
5. Optional: the \`SetPreferredAppMode\` dark-menu experiment; \`overlay_timer\` and \`overlay_position\` in the config file and README.

## 6. Open questions

1. Should the error badge persist until acknowledged (opening History), or clear on the next successful dictation? The plan assumes it clears on either.
2. In realtime mode, should the overlay show a hint of the latest partial transcript (a tooltip-like line above the pill)? It is useful for long dictations but doubles the overlay size. Out of scope unless you want it.
3. Should push-to-talk mode show a different dot (for example, hollow) so it is clear that releasing the key stops recording?


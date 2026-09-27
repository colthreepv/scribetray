# Scribetray M0 Rust spike

This Windows-only CLI is a caret discovery and guarded paste spike. It
uses the official `windows` crate APIs and has no dependency on the repository's
other application components.

## Build and run

From this directory in PowerShell:

```powershell
cargo check
cargo build --release
cargo run --release
```

The host needs the Rust MSVC toolchain and Windows SDK. The binary stays attached
to its console so its diagnostics remain visible.

## Try it

1. Focus a text editor or another application with a writable text field.
2. Press **Win+Alt+V** once. The CLI prints the foreground top-level window,
   focused HWND, and the best caret rectangle it can obtain.
3. Press **Win+Alt+V** again to paste the visible marker `[ScribeTray]` at the
   current caret. The spike temporarily places the marker on the clipboard,
   sends Ctrl+V, then restores the prior clipboard OLE data object. It does not
   press Enter.
4. Press **Escape** while the snapshot is armed to cancel it.

The second press pastes only when both the foreground top-level HWND and the
focused HWND still match the first snapshot. When UI Automation provides the
focused element's RuntimeId, that value is also compared so a different field
inside the same window is rejected. If RuntimeId is unavailable, the HWND
comparison remains the fallback. A mismatch clears the pending snapshot and
refuses the paste. The spike waits up to one second for the Win+Alt+V keys to
be released before it sends Ctrl+V. It snapshots the clipboard through
`OleGetClipboard` before changing it; if that snapshot fails, it refuses to
overwrite the clipboard.

## Caret lookup order

The CLI tries these methods in order and prints the selected source and method:

1. `GetGUIThreadInfo` and its `rcCaret`, converting from the caret window's
   client/logical coordinates to screen coordinates with `ClientToScreen`.
2. MSAA `AccessibleObjectFromWindow(OBJID_CARET)` and `IAccessible::accLocation`.
3. UI Automation `TextPattern2::GetCaretRange` and
   `IUIAutomationTextRange::GetBoundingRectangles`.
4. `GetCursorPos` as an explicitly labeled mouse-position fallback. This is an
   approximation and is not reported as an actual caret rectangle.

## Known limitations

- Applications expose caret state inconsistently. A provider may omit the
  system caret, MSAA caret object, or UI Automation TextPattern2; the mouse
  fallback then reports only the pointer position.
- `TextPattern2::GetCaretRange` returns a zero-length range. Some app providers
  return no bounding rectangles for that range; the CLI reports this result
  explicitly and continues to the mouse fallback. This behavior can vary by
  application/provider.
- UI Automation rectangles are returned as screen coordinates and may also be
  empty for hidden, obscured, or off-screen carets. Cross-process provider calls
  can be slow or unavailable.
- RuntimeIds are opaque, may be unavailable from a provider, and can be reused
  over time. The HWND focus check remains active as a fallback.
- Paste uses `SendInput` for Ctrl+V, which can be blocked by integrity-level
  boundaries or rejected by the target application. The focus check reduces
  misdelivery but cannot make checking and pasting atomic.
- The previous clipboard `IDataObject` is restored with `OleSetClipboard` after
  a short paste delay. If OLE restoration fails, the spike falls back to
  restoring Unicode text only (or clearing the clipboard when there was no
  Unicode text); rich formats may be lost in that failure case.
- The marker is deliberately visible and remains in the document until removed
  by the user. Do not use the paste step in a document where that marker is
  undesirable.
- The global hotkey may already be registered by another application. In that
  case startup reports the Windows API error and exits.
- The Escape hook observes Escape only while a snapshot is pending and passes
  the key on to the foreground application after cancelling.

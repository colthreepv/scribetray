//! Windows focus, caret, clipboard, and text input helpers.
//!
//! Paste mode snapshots a small whitelist of HGLOBAL clipboard formats as owned
//! bytes (up to 64 MiB), then restores them only while our temporary text remains.
//! Temporary transcripts opt out of clipboard managers/history; fallback copies
//! remain ordinary CF_UNICODETEXT. GDI handles and arbitrary formats are not saved.

use std::mem::{ManuallyDrop, size_of};
use std::ptr;
use std::slice;
use std::thread::sleep;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{info, warn};
use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HANDLE, HGLOBAL, HINSTANCE, HWND, POINT, SetLastError,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, SAFEARRAY};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT, OleInitialize, OleUninitialize,
    SafeArrayCreateVector, SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement,
    SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayPutElement,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_I4,
};
use windows::Win32::UI::Accessibility::{
    AccessibleObjectFromWindow, CUIAutomation, IAccessible, IUIAutomation, IUIAutomationElement,
    IUIAutomationTextPattern, IUIAutomationTextPattern2, IUIAutomationTextRange,
    IUIAutomationValuePattern, TreeScope_Descendants, UIA_CONTROLTYPE_ID,
    UIA_ComboBoxControlTypeId, UIA_EditControlTypeId, UIA_IsValuePatternAvailablePropertyId,
    UIA_RuntimeIdPropertyId, UIA_TextPattern2Id, UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_LMENU,
    VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RETURN, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DestroyWindow, GUITHREADINFO, GetCursorPos, GetForegroundWindow,
    GetGUIThreadInfo, GetWindowThreadProcessId, OBJID_CARET, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{Interface, PCWSTR};
use windows::{core::Error as WindowsError, core::Result as WindowsResult};

/// The way a caret-like anchor rectangle was obtained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaretMethod {
    /// `GetGUIThreadInfo::rcCaret`, converted from the caret window's client space.
    GuiThreadInfo,
    /// MSAA `OBJID_CARET` through the `hwndCaret` supplied by `GetGUIThreadInfo`.
    Msaa,
    /// UI Automation `TextPattern2` or a collapsed `TextPattern` selection.
    UiaTextPattern,
    /// Mouse position; this is only an anchor fallback, not a detected caret.
    MouseFallback,
}

/// A rectangle in screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaretRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// A caret or fallback anchor and the API that produced it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaretSnapshot {
    pub rect: CaretRect,
    pub method: CaretMethod,
}

/// Foreground and focused target identity captured before dictation starts.
#[derive(Clone, Debug)]
pub struct TargetSnapshot {
    pub foreground: HWND,
    pub focused: HWND,
    /// UIA identity distinguishes controls hosted by a single HWND (for example,
    /// separate editor fields inside a Chromium window).
    pub focused_runtime_id: Option<Vec<i32>>,
    pub caret: Option<CaretSnapshot>,
}

#[derive(Debug)]
pub enum RefocusOutcome {
    Refocused,
    OtherWindow,
    NoRuntimeId,
    UserEditing,
    OriginalGone,
    OriginalOffscreen,
    RefocusFailed,
    Failed,
}

/// Restore the captured element only when focus moved within its window to a non-editor.
pub fn refocus_original(target: &TargetSnapshot) -> RefocusOutcome {
    if unsafe { GetForegroundWindow() } != target.foreground {
        return RefocusOutcome::OtherWindow;
    }
    let Some(id) = target.focused_runtime_id.as_deref() else {
        return RefocusOutcome::NoRuntimeId;
    };
    match refocus_original_inner(target, id) {
        Ok(outcome) => outcome,
        Err(_) => RefocusOutcome::Failed,
    }
}

fn refocus_original_inner(target: &TargetSnapshot, id: &[i32]) -> WindowsResult<RefocusOutcome> {
    let _com = ComApartment::initialize()?;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    let focused = unsafe { automation.GetFocusedElement()? };
    if focused_is_editable(&focused)? {
        return Ok(RefocusOutcome::UserEditing);
    }

    let invalid_argument =
        || WindowsError::from_hresult(windows::core::HRESULT(0x80070057_u32 as i32));
    let count = u32::try_from(id.len()).map_err(|_| invalid_argument())?;
    let array = unsafe { SafeArrayCreateVector(VT_I4, 0, count) };
    if array.is_null() {
        return Err(WindowsError::from_hresult(windows::core::HRESULT(
            0x8007000e_u32 as i32,
        )));
    }
    let guard = SafeArrayGuard(array);
    for (index, value) in id.iter().enumerate() {
        let index = i32::try_from(index).map_err(|_| invalid_argument())?;
        unsafe { SafeArrayPutElement(guard.0, &index, (value as *const i32).cast())? };
    }
    // VARIANT owns the SAFEARRAY and clears it when dropped.
    let value = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VARENUM(VT_ARRAY.0 | VT_I4.0),
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { parray: guard.0 },
            }),
        },
    };
    std::mem::forget(guard);
    let root = unsafe { automation.ElementFromHandle(target.foreground)? };
    let condition = unsafe { automation.CreatePropertyCondition(UIA_RuntimeIdPropertyId, &value)? };
    // The generated FindFirst wrapper maps a successful null result to an error;
    // call the vtable to distinguish an absent element from an API failure.
    let mut raw = ptr::null_mut();
    unsafe {
        (Interface::vtable(&root).FindFirst)(
            Interface::as_raw(&root),
            TreeScope_Descendants,
            Interface::as_raw(&condition),
            &mut raw,
        )
        .ok()?;
    }
    if raw.is_null() {
        return Ok(RefocusOutcome::OriginalGone);
    }
    let original = unsafe { IUIAutomationElement::from_raw(raw) };
    if unsafe { original.CurrentIsOffscreen()? }.as_bool() {
        return Ok(RefocusOutcome::OriginalOffscreen);
    }
    if unsafe { GetForegroundWindow() } != target.foreground {
        return Ok(RefocusOutcome::OtherWindow);
    }
    if focused_is_editable(&unsafe { automation.GetFocusedElement()? })? {
        return Ok(RefocusOutcome::UserEditing);
    }
    if unsafe { GetForegroundWindow() } != target.foreground {
        return Ok(RefocusOutcome::OtherWindow);
    }
    unsafe { original.SetFocus()? };
    sleep(Duration::from_millis(50));
    Ok(if target_is_current_inner(target) {
        RefocusOutcome::Refocused
    } else {
        RefocusOutcome::RefocusFailed
    })
}

fn focused_is_editable(element: &IUIAutomationElement) -> WindowsResult<bool> {
    let control_type = unsafe { element.CurrentControlType()? };
    if editable_control_type(control_type) {
        return Ok(true);
    }
    // Chromium focuses its root web area (a Document) when the user clicks empty
    // page space; a Document counts as an editor only with a writable value.
    let available =
        unsafe { element.GetCurrentPropertyValue(UIA_IsValuePatternAvailablePropertyId)? };
    if !bool::try_from(&available)? {
        return Ok(false);
    }
    let pattern: IUIAutomationValuePattern =
        unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId)? };
    Ok(!unsafe { pattern.CurrentIsReadOnly()? }.as_bool())
}

fn editable_control_type(control_type: UIA_CONTROLTYPE_ID) -> bool {
    control_type == UIA_EditControlTypeId || control_type == UIA_ComboBoxControlTypeId
}

/// Text insertion strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertMethod {
    /// Put Unicode text on the clipboard and synthesize Ctrl+V.
    Paste,
    /// Synthesize Unicode keyboard events directly.
    UnicodeTyping,
}

/// Failures reported by this module.
#[derive(Debug, Error)]
pub enum WinInputError {
    #[error("Windows API call failed: {0}")]
    Windows(#[from] WindowsError),
    #[error("there is no foreground window to snapshot")]
    NoForegroundWindow,
    #[error("the foreground window has no focused HWND")]
    NoFocusedWindow,
    #[error("the target window or focused element changed before insertion")]
    TargetChanged,
    #[error("the hotkey modifiers were still held after waiting one second")]
    HotkeyStillHeld,
    #[error("SendInput accepted {sent} of {expected} keyboard events")]
    PartialInput { sent: u32, expected: u32 },
    #[error("the clipboard owner window could not be created")]
    ClipboardOwnerUnavailable,
    #[error("the clipboard text is too large to allocate")]
    ClipboardTextTooLarge,
}

type Result<T> = std::result::Result<T, WinInputError>;

/// Capture foreground/focus identity and the best available caret anchor.
pub fn capture_target() -> Result<TargetSnapshot> {
    let _com = ComApartment::initialize()?;
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return Err(WinInputError::NoForegroundWindow);
    }

    let focused_element = focused_uia_element().ok();
    let focused_runtime_id = focused_element
        .as_ref()
        .and_then(|element| runtime_id(element).ok().flatten());
    let gui = gui_thread_info(foreground).ok();
    let focused = gui.as_ref().map(|info| info.hwndFocus).unwrap_or_default();
    if focused.0.is_null() && focused_runtime_id.is_none() {
        return Err(WinInputError::NoFocusedWindow);
    }

    let caret = resolve_caret(gui.as_ref(), focused_element.as_ref());

    Ok(TargetSnapshot {
        foreground,
        focused,
        focused_runtime_id,
        caret,
    })
}

/// Refreshes the caret only while the original foreground and focused control
/// are still active. A changed target returns `None` and leaves the saved
/// anchor at its last known location.
pub fn refresh_caret(target: &TargetSnapshot) -> Option<CaretSnapshot> {
    let current = capture_target().ok()?;
    if !same_input_target(target, &current) {
        return None;
    }
    current.caret
}

fn same_input_target(expected: &TargetSnapshot, current: &TargetSnapshot) -> bool {
    if expected.foreground != current.foreground {
        return false;
    }
    if !expected.focused.0.is_null() && expected.focused != current.focused {
        return false;
    }
    match expected.focused_runtime_id.as_deref() {
        Some(runtime_id) => current.focused_runtime_id.as_deref() == Some(runtime_id),
        None => !expected.focused.0.is_null() && expected.focused == current.focused,
    }
}

/// Insert text only if the captured focus is still current.
///
/// For paste mode, supported prior clipboard formats are restored after a short
/// delay to let the target consume Ctrl+V when `restore_clipboard` is true.
pub fn insert_text(
    target: &TargetSnapshot,
    text: &str,
    method: InsertMethod,
    restore_clipboard: bool,
) -> Result<()> {
    wait_for_hotkey_release()?;
    let _com = ComApartment::initialize()?;
    if !target_is_current_inner(target) {
        return Err(WinInputError::TargetChanged);
    }

    match method {
        InsertMethod::UnicodeTyping => send_unicode_text(text),
        InsertMethod::Paste => paste_text(text, restore_clipboard),
    }
}

/// Replace clipboard contents with Unicode text, for use when the focus guard fails.
pub fn copy_text(text: &str) -> Result<()> {
    let owner = ClipboardOwner::create()?;
    set_clipboard_text(owner.hwnd, text, false)
}

/// Returns true while the captured window and focused element are active again.
pub fn target_is_current(target: &TargetSnapshot) -> bool {
    match ComApartment::initialize() {
        Ok(_com) => target_is_current_inner(target),
        Err(_) => false,
    }
}

/// Synthesize Enter. This module emits Enter nowhere else.
pub fn send_enter(target: &TargetSnapshot) -> Result<()> {
    wait_for_hotkey_release()?;
    let _com = ComApartment::initialize()?;
    if !target_window_and_focus_are_current(target) {
        return Err(WinInputError::TargetChanged);
    }
    let inputs = [
        key_input(VK_RETURN, KEYBD_EVENT_FLAGS(0)),
        key_input(VK_RETURN, KEYEVENTF_KEYUP),
    ];
    send_inputs(&inputs)
}

// Some Chromium and Electron editors recreate their UIA text element when a
// paste changes the document. For Enter, the foreground window and focused
// HWND still provide a useful guard while allowing that UIA RuntimeId to change.
fn target_window_and_focus_are_current(target: &TargetSnapshot) -> bool {
    if target.foreground.0.is_null()
        || (target.focused.0.is_null() && target.focused_runtime_id.is_none())
        || unsafe { GetForegroundWindow() } != target.foreground
    {
        return false;
    }

    let current_gui = gui_thread_info(target.foreground).ok();
    let current_focused = current_gui.as_ref().map(|info| info.hwndFocus);
    if !target.focused.0.is_null() {
        return current_focused == Some(target.focused);
    }

    match target.focused_runtime_id.as_deref() {
        Some(expected) => focused_uia_element()
            .ok()
            .and_then(|element| runtime_id(&element).ok().flatten())
            .is_some_and(|current| current == expected),
        None => false,
    }
}

fn paste_text(text: &str, restore_clipboard: bool) -> Result<()> {
    let owner = ClipboardOwner::create()?;
    let backup = if restore_clipboard {
        ClipboardBackup::capture().unwrap_or_else(|error| {
            warn!("could not capture clipboard for preservation: {error}");
            None
        })
    } else {
        None
    };

    let insert_result = set_clipboard_text(owner.hwnd, text, true).and_then(|()| {
        send_ctrl_v()?;
        sleep(Duration::from_millis(200));
        Ok(())
    });

    if let Some(backup) = backup {
        match backup.restore_if_still_owned(owner.hwnd, text) {
            Ok(true) => info!("clipboard restored after paste"),
            Ok(false) => warn!("clipboard changed during paste; preserving its newer contents"),
            Err(error) => warn!("text was pasted but clipboard restore failed: {error}"),
        }
    }

    insert_result
}

fn target_is_current_inner(target: &TargetSnapshot) -> bool {
    if target.foreground.0.is_null()
        || (target.focused.0.is_null() && target.focused_runtime_id.is_none())
    {
        return false;
    }
    let current_foreground = unsafe { GetForegroundWindow() };
    if current_foreground != target.foreground {
        return false;
    }
    let current_gui = gui_thread_info(current_foreground).ok();
    let current_focused = current_gui.as_ref().map(|info| info.hwndFocus);
    if !target.focused.0.is_null() && current_focused != Some(target.focused) {
        return false;
    }

    match target.focused_runtime_id.as_deref() {
        Some(expected) => focused_uia_element()
            .ok()
            .and_then(|element| runtime_id(&element).ok().flatten())
            .is_some_and(|current| current == expected),
        None => !target.focused.0.is_null() && current_focused == Some(target.focused),
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> WindowsResult<Self> {
        unsafe { OleInitialize(None)? };
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

fn gui_thread_info(foreground: HWND) -> WindowsResult<GUITHREADINFO> {
    let thread_id = unsafe { GetWindowThreadProcessId(foreground, None) };
    if thread_id == 0 {
        return Err(WindowsError::from_thread());
    }
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(thread_id, &mut info)? };
    Ok(info)
}

fn focused_uia_element() -> WindowsResult<IUIAutomationElement> {
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    unsafe { automation.GetFocusedElement() }
}

fn runtime_id(element: &IUIAutomationElement) -> WindowsResult<Option<Vec<i32>>> {
    let array = unsafe { element.GetRuntimeId()? };
    if array.is_null() {
        return Ok(None);
    }
    let guard = SafeArrayGuard(array);
    if unsafe { SafeArrayGetDim(guard.0) } != 1 {
        return Ok(None);
    }
    let lower = unsafe { SafeArrayGetLBound(guard.0, 1)? };
    let upper = unsafe { SafeArrayGetUBound(guard.0, 1)? };
    if upper < lower || (upper as i64 - lower as i64) > 1023 {
        return Ok(None);
    }
    let mut values = Vec::with_capacity((upper - lower + 1) as usize);
    for index in lower..=upper {
        let mut value = 0_i32;
        unsafe {
            SafeArrayGetElement(guard.0, &index, (&mut value as *mut i32).cast())?;
        }
        values.push(value);
    }
    Ok((!values.is_empty()).then_some(values))
}

struct SafeArrayGuard(*mut SAFEARRAY);

impl Drop for SafeArrayGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = SafeArrayDestroy(self.0);
            }
        }
    }
}

fn resolve_caret(
    gui: Option<&GUITHREADINFO>,
    focused_element: Option<&IUIAutomationElement>,
) -> Option<CaretSnapshot> {
    if let Some(gui) = gui {
        if let Some(rect) = gui_caret_rect(gui.hwndCaret, gui.rcCaret) {
            return Some(CaretSnapshot {
                rect,
                method: CaretMethod::GuiThreadInfo,
            });
        }
        if !gui.hwndCaret.0.is_null() {
            if let Ok(Some(rect)) = msaa_caret_rect(gui.hwndCaret) {
                return Some(CaretSnapshot {
                    rect,
                    method: CaretMethod::Msaa,
                });
            }
        }
        // Some Chromium/Electron controls expose OBJID_CARET on the focused
        // child even when GetGUIThreadInfo reports no dedicated caret HWND.
        if !gui.hwndFocus.0.is_null()
            && gui.hwndFocus != gui.hwndCaret
            && let Ok(Some(rect)) = msaa_caret_rect(gui.hwndFocus)
        {
            return Some(CaretSnapshot {
                rect,
                method: CaretMethod::Msaa,
            });
        }
    }
    if let Some(element) = focused_element {
        if let Ok(Some(rect)) = uia_caret_rect(element) {
            return Some(CaretSnapshot {
                rect,
                method: CaretMethod::UiaTextPattern,
            });
        }
    }

    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point).ok()? };
    Some(CaretSnapshot {
        rect: CaretRect {
            left: point.x as f64,
            top: point.y as f64,
            width: 1.0,
            height: 1.0,
        },
        method: CaretMethod::MouseFallback,
    })
}

fn gui_caret_rect(
    caret_window: HWND,
    client_rect: windows::Win32::Foundation::RECT,
) -> Option<CaretRect> {
    if caret_window.0.is_null() {
        return None;
    }
    let mut top_left = POINT {
        x: client_rect.left,
        y: client_rect.top,
    };
    let mut bottom_right = POINT {
        x: client_rect.right,
        y: client_rect.bottom,
    };
    if !unsafe { ClientToScreen(caret_window, &mut top_left).as_bool() }
        || !unsafe { ClientToScreen(caret_window, &mut bottom_right).as_bool() }
    {
        return None;
    }
    let rect = CaretRect {
        left: top_left.x as f64,
        top: top_left.y as f64,
        width: (bottom_right.x - top_left.x) as f64,
        height: (bottom_right.y - top_left.y) as f64,
    };
    valid_caret_rect(rect).then_some(rect)
}

fn msaa_caret_rect(caret_window: HWND) -> WindowsResult<Option<CaretRect>> {
    let mut raw_accessible = ptr::null_mut();
    unsafe {
        AccessibleObjectFromWindow(
            caret_window,
            OBJID_CARET.0 as u32,
            &IAccessible::IID,
            &mut raw_accessible,
        )?;
    }
    let accessible = unsafe { IAccessible::from_raw(raw_accessible.cast()) };
    let child = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    lVal: CHILDID_SELF as i32,
                },
            }),
        },
    };
    let (mut left, mut top, mut width, mut height) = (0, 0, 0, 0);
    unsafe { accessible.accLocation(&mut left, &mut top, &mut width, &mut height, &child)? };
    let rect = CaretRect {
        left: left as f64,
        top: top as f64,
        width: width as f64,
        height: height as f64,
    };
    Ok(valid_caret_rect(rect).then_some(rect))
}

fn uia_caret_rect(element: &IUIAutomationElement) -> WindowsResult<Option<CaretRect>> {
    if let Ok(text_pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) }
    {
        let mut is_active = windows::core::BOOL(0);
        if let Ok(range) = unsafe { text_pattern.GetCaretRange(&mut is_active) }
            && is_active.as_bool()
            && let Ok(Some(rect)) = text_range_rect(&range)
        {
            return Ok(Some(rect));
        }
    }

    // Some Chromium providers expose TextPattern but return S_OK with a null
    // TextPattern2 caret range. A collapsed selection can still expose the
    // caret rectangle through the base pattern.
    let text_pattern: IUIAutomationTextPattern =
        unsafe { element.GetCurrentPatternAs(UIA_TextPatternId)? };
    let selection = unsafe { text_pattern.GetSelection()? };
    if unsafe { selection.Length()? } != 1 {
        return Ok(None);
    }
    let range = unsafe { selection.GetElement(0)? };
    if !unsafe { range.GetText(1)? }.is_empty() {
        return Ok(None);
    }
    text_range_rect(&range)
}

fn text_range_rect(range: &IUIAutomationTextRange) -> WindowsResult<Option<CaretRect>> {
    let array = unsafe { range.GetBoundingRectangles()? };
    if array.is_null() {
        return Ok(None);
    }
    let guard = SafeArrayGuard(array);
    if unsafe { SafeArrayGetDim(guard.0) } != 1 {
        return Ok(None);
    }
    let lower = unsafe { SafeArrayGetLBound(guard.0, 1)? };
    let upper = unsafe { SafeArrayGetUBound(guard.0, 1)? };
    if upper < lower || (upper as i64 - lower as i64) < 3 {
        return Ok(None);
    }
    let mut bounds = [0.0_f64; 4];
    for (offset, value) in bounds.iter_mut().enumerate() {
        let index = lower + offset as i32;
        unsafe { SafeArrayGetElement(guard.0, &index, (value as *mut f64).cast())? };
    }
    let rect = CaretRect {
        left: bounds[0],
        top: bounds[1],
        width: bounds[2],
        height: bounds[3],
    };
    Ok(valid_caret_rect(rect).then_some(rect))
}

fn valid_caret_rect(rect: CaretRect) -> bool {
    rect.left.is_finite()
        && rect.top.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width >= 0.0
        && rect.height > 0.0
}

fn wait_for_hotkey_release() -> Result<()> {
    let keys = [
        VK_V,
        VK_LWIN,
        VK_RWIN,
        VK_LMENU,
        VK_RMENU,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_RETURN,
    ];
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let held = keys
            .iter()
            .any(|key| unsafe { GetAsyncKeyState(key.0 as i32) } < 0);
        if !held {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(WinInputError::HotkeyStillHeld);
        }
        sleep(Duration::from_millis(8));
    }
}

fn send_unicode_text(text: &str) -> Result<()> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut inputs = Vec::with_capacity(units.len().saturating_mul(2));
    for unit in units.iter().copied() {
        inputs.push(unicode_input(unit, KEYBD_EVENT_FLAGS(0)));
        inputs.push(unicode_input(unit, KEYEVENTF_KEYUP));
    }
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        return Ok(());
    }
    if sent % 2 == 1 {
        let release = [unicode_input(units[(sent / 2) as usize], KEYEVENTF_KEYUP)];
        let _ = unsafe { SendInput(&release, size_of::<INPUT>() as i32) };
    }
    Err(WinInputError::PartialInput {
        sent,
        expected: inputs.len() as u32,
    })
}

fn send_ctrl_v() -> Result<()> {
    let inputs = [
        key_input(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        key_input(VK_V, KEYBD_EVENT_FLAGS(0)),
        key_input(VK_V, KEYEVENTF_KEYUP),
        key_input(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        return Ok(());
    }
    // A short SendInput can leave a modifier logically down. Best-effort release.
    let releases = [
        key_input(VK_V, KEYEVENTF_KEYUP),
        key_input(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    let _ = unsafe { SendInput(&releases, size_of::<INPUT>() as i32) };
    Err(WinInputError::PartialInput {
        sent,
        expected: inputs.len() as u32,
    })
}

fn send_inputs(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err(WinInputError::PartialInput {
            sent,
            expected: inputs.len() as u32,
        })
    }
}

fn unicode_input(unit: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(key: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

struct ClipboardOwner {
    hwnd: HWND,
}

impl ClipboardOwner {
    fn create() -> Result<Self> {
        let instance = unsafe { GetModuleHandleW(None)? };
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                windows::core::w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(HINSTANCE(instance.0)),
                None,
            )
        }
        .map_err(|_| WinInputError::ClipboardOwnerUnavailable)?;
        Ok(Self { hwnd })
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

struct ClipboardOpen;

impl ClipboardOpen {
    fn open(owner: Option<HWND>) -> WindowsResult<Self> {
        for attempt in 0..10 {
            match unsafe { OpenClipboard(owner) } {
                Ok(()) => return Ok(Self),
                Err(error) if attempt == 9 => return Err(error),
                Err(_) => sleep(Duration::from_millis(12)),
            }
        }
        unreachable!()
    }
}

impl Drop for ClipboardOpen {
    fn drop(&mut self) {
        unsafe {
            if let Err(error) = CloseClipboard() {
                // Published data is already owned by Windows. Do not report a
                // successful paste as failed because a listener raced the close.
                warn!("CloseClipboard failed after clipboard access: {error}");
            }
        }
    }
}

const CLIPBOARD_SNAPSHOT_LIMIT: usize = 64 * 1024 * 1024;
const SNAPSHOT_REGISTERED_FORMATS: [PCWSTR; 4] = [
    windows::core::w!("HTML Format"),
    windows::core::w!("Rich Text Format"),
    windows::core::w!("PNG"),
    windows::core::w!("Preferred DropEffect"),
];

fn register_clipboard_format(name: PCWSTR) -> WindowsResult<u32> {
    let format = unsafe { RegisterClipboardFormatW(name) };
    if format == 0 {
        Err(WindowsError::from_thread())
    } else {
        Ok(format)
    }
}

fn snapshot_format_allowed(format: u32, registered: &[u32]) -> bool {
    [CF_UNICODETEXT, CF_DIB, CF_DIBV5, CF_HDROP]
        .iter()
        .any(|allowed| format == u32::from(allowed.0))
        || (format >= 0xc000 && registered.contains(&format))
}

fn snapshot_size_after(total: usize, next: usize) -> Option<usize> {
    total
        .checked_add(next)
        .filter(|size| *size <= CLIPBOARD_SNAPSHOT_LIMIT)
}

struct ClipboardBackup {
    // An empty vector represents an originally empty clipboard.
    formats: Vec<(u32, Vec<u8>)>,
}

impl ClipboardBackup {
    fn capture() -> WindowsResult<Option<Self>> {
        let registered = SNAPSHOT_REGISTERED_FORMATS
            .map(register_clipboard_format)
            .into_iter()
            .collect::<WindowsResult<Vec<_>>>()?;
        let _clipboard = ClipboardOpen::open(None)?;
        // Enumerate before requesting data: GetClipboardData can trigger delayed
        // rendering in the source app. Never request untrusted handle formats.
        let mut available = Vec::new();
        let mut format = 0;
        loop {
            unsafe { SetLastError(ERROR_SUCCESS) };
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                if unsafe { GetLastError() } != ERROR_SUCCESS {
                    return Err(WindowsError::from_thread());
                }
                break;
            }
            available.push(format);
        }
        let was_empty = available.is_empty();
        let mut formats = Vec::new();
        let mut total = 0;
        for format in available
            .into_iter()
            .filter(|id| snapshot_format_allowed(*id, &registered))
        {
            let handle = match unsafe { GetClipboardData(format) } {
                Ok(handle) => HGLOBAL(handle.0),
                Err(error) => {
                    warn!("could not read clipboard format {format}: {error}");
                    continue;
                }
            };
            let size = unsafe { GlobalSize(handle) };
            let Some(new_total) = snapshot_size_after(total, size) else {
                info!(
                    "clipboard was too large to preserve (over 64 MiB); leaving transcript on clipboard"
                );
                return Ok(None);
            };
            let locked = unsafe { GlobalLock(handle) };
            if locked.is_null() {
                let error = WindowsError::from_thread();
                warn!("could not lock clipboard format {format}: {error}");
                continue;
            }
            let bytes = unsafe { slice::from_raw_parts(locked.cast::<u8>(), size) }.to_vec();
            let _ = unsafe { GlobalUnlock(handle) };
            formats.push((format, bytes));
            total = new_total;
        }
        if !was_empty && formats.is_empty() {
            info!(
                "clipboard has no supported formats to preserve; leaving transcript on clipboard"
            );
            return Ok(None);
        }
        Ok(Some(Self { formats }))
    }

    fn restore_if_still_owned(
        self,
        temporary_owner: HWND,
        pasted_text: &str,
    ) -> WindowsResult<bool> {
        // Check and restore under the same lock so a newer copy cannot slip in
        // between the ownership/text comparison and EmptyClipboard.
        let _clipboard = ClipboardOpen::open(Some(temporary_owner))?;
        // A source process can exit while its clipboard data remains. The
        // windows-rs wrapper reports a null owner as Err (with possibly stale
        // last-error state); it still needs the text comparison in that case.
        let still_contains_pasted_text = unsafe { GetClipboardOwner() }.ok()
            == Some(temporary_owner)
            || clipboard_text_matches(pasted_text);
        if !still_contains_pasted_text {
            // Another application has replaced our temporary text with newer
            // clipboard contents; leave those contents intact.
            return Ok(false);
        }
        // Allocate all replacement blocks before discarding the current data.
        let mut prepared = self
            .formats
            .iter()
            .map(|(format, bytes)| Ok((*format, GlobalMemory::copy_from(bytes)?)))
            .collect::<WindowsResult<Vec<_>>>()?;
        unsafe { EmptyClipboard()? };
        publish_clipboard_markers(false);
        for (format, memory) in &mut prepared {
            memory.publish(*format)?;
        }
        Ok(true)
    }
}

fn clipboard_text_matches(expected: &str) -> bool {
    // Caller holds ClipboardOpen for the entire check/restore transaction.
    let Ok(handle) = (unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }) else {
        return false;
    };
    let memory = HGLOBAL(handle.0);
    let size_bytes = unsafe { GlobalSize(memory) };
    if size_bytes < size_of::<u16>() || size_bytes % size_of::<u16>() != 0 {
        return false;
    }
    let locked = unsafe { GlobalLock(memory) };
    if locked.is_null() {
        return false;
    }
    let words =
        unsafe { slice::from_raw_parts(locked.cast::<u16>(), size_bytes / size_of::<u16>()) };
    let matches = words
        .iter()
        .position(|word| *word == 0)
        .is_some_and(|length| words[..length].iter().copied().eq(expected.encode_utf16()));
    let _ = unsafe { GlobalUnlock(memory) };
    matches
}

fn set_clipboard_text(owner: HWND, text: &str, temporary: bool) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = utf16
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or(WinInputError::ClipboardTextTooLarge)?;

    let mut memory = GlobalMemory::copy_from(unsafe {
        slice::from_raw_parts(utf16.as_ptr().cast::<u8>(), bytes)
    })?;

    let _clipboard = ClipboardOpen::open(Some(owner))?;
    unsafe { EmptyClipboard()? };
    if temporary {
        publish_clipboard_markers(true);
    }
    memory.publish(CF_UNICODETEXT.0 as u32)?;
    Ok(())
}

// Caller holds the clipboard open. Restore only opts out of monitor processing;
// temporary transcripts also opt out of Windows history and cloud sync.
fn publish_clipboard_markers(temporary: bool) {
    let monitor_markers = [
        windows::core::w!("Clipboard Viewer Ignore"),
        windows::core::w!("ExcludeClipboardContentFromMonitorProcessing"),
    ];
    let temporary_markers = [
        windows::core::w!("CanIncludeInClipboardHistory"),
        windows::core::w!("CanUploadToCloudClipboard"),
    ];
    for name in
        monitor_markers
            .into_iter()
            .chain(
                temporary_markers
                    .into_iter()
                    .take(if temporary { 2 } else { 0 }),
            )
    {
        let result = register_clipboard_format(name)
            .and_then(|format| GlobalMemory::copy_from(&0_u32.to_ne_bytes())?.publish(format));
        if let Err(error) = result {
            warn!("could not set clipboard marker {name:?}: {error}");
        }
    }
}

struct GlobalMemory {
    handle: HGLOBAL,
    transferred: bool,
}

impl GlobalMemory {
    fn copy_from(bytes: &[u8]) -> WindowsResult<Self> {
        let memory = Self {
            handle: unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))? },
            transferred: false,
        };
        let locked = unsafe { GlobalLock(memory.handle) };
        if locked.is_null() {
            return Err(WindowsError::from_thread());
        }
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), locked.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(memory.handle);
        }
        Ok(memory)
    }

    fn publish(&mut self, format: u32) -> WindowsResult<()> {
        unsafe { SetClipboardData(format, Some(HANDLE(self.handle.0)))? };
        self.transferred = true;
        Ok(())
    }
}

impl Drop for GlobalMemory {
    fn drop(&mut self) {
        if !self.transferred {
            unsafe {
                let _ = windows::Win32::Foundation::GlobalFree(Some(self.handle));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows::Win32::System::Ole::{
        CF_BITMAP, CF_ENHMETAFILE, CF_LOCALE, CF_METAFILEPICT, CF_OEMTEXT, CF_PALETTE, CF_TEXT,
    };

    #[test]
    fn editable_control_types_include_standard_editors() {
        use windows::Win32::UI::Accessibility::UIA_DocumentControlTypeId;
        assert!(editable_control_type(UIA_EditControlTypeId));
        assert!(editable_control_type(UIA_ComboBoxControlTypeId));
        assert!(!editable_control_type(UIA_DocumentControlTypeId));
        assert!(!editable_control_type(UIA_CONTROLTYPE_ID(50000)));
    }

    #[test]
    fn snapshot_whitelist_only_allows_supported_hglobal_formats() {
        let registered = [0xc001, 0xc002, 0xc003, 0xc004];
        for format in [CF_UNICODETEXT, CF_DIB, CF_DIBV5, CF_HDROP] {
            assert!(snapshot_format_allowed(u32::from(format.0), &registered));
        }
        for format in registered {
            assert!(snapshot_format_allowed(format, &registered));
        }
        for format in [
            CF_BITMAP,
            CF_ENHMETAFILE,
            CF_METAFILEPICT,
            CF_PALETTE,
            CF_TEXT,
            CF_OEMTEXT,
            CF_LOCALE,
        ] {
            assert!(!snapshot_format_allowed(u32::from(format.0), &registered));
        }
        for format in [0, 0xc005, u32::MAX] {
            assert!(!snapshot_format_allowed(format, &registered));
        }
        // Registration failure (0) must never accidentally whitelist a format.
        assert!(!snapshot_format_allowed(0, &[0]));
    }

    #[test]
    fn snapshot_cap_is_inclusive_and_checks_total_and_overflow() {
        assert_eq!(snapshot_size_after(0, 0), Some(0));
        assert_eq!(
            snapshot_size_after(0, CLIPBOARD_SNAPSHOT_LIMIT),
            Some(CLIPBOARD_SNAPSHOT_LIMIT)
        );
        assert_eq!(
            snapshot_size_after(CLIPBOARD_SNAPSHOT_LIMIT - 4, 4),
            Some(CLIPBOARD_SNAPSHOT_LIMIT)
        );
        assert_eq!(snapshot_size_after(CLIPBOARD_SNAPSHOT_LIMIT, 1), None);
        assert_eq!(snapshot_size_after(1, CLIPBOARD_SNAPSHOT_LIMIT), None);
        assert_eq!(snapshot_size_after(0, CLIPBOARD_SNAPSHOT_LIMIT + 1), None);
        assert_eq!(snapshot_size_after(usize::MAX, 1), None);
    }

    fn powershell(script: &str) -> String {
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn clipboard probe");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().expect("poll clipboard probe").is_some() {
                let output = child.wait_with_output().expect("read clipboard probe");
                assert!(
                    output.status.success(),
                    "PowerShell failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                return String::from_utf8(output.stdout).expect("UTF-8 probe output");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("clipboard probe exceeded its 10-second deadline");
            }
            sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "uses the real Windows clipboard and PowerShell child processes"]
    fn live_clipboard_snapshot_roundtrip() {
        let owner = ClipboardOwner::create().expect("clipboard owner");
        for round in 1..=8 {
            let original = format!("ORIGINAL-{}-{round}", std::process::id());
            powershell(&format!("Set-Clipboard '{original}'"));
            let backup = ClipboardBackup::capture().unwrap().expect("text snapshot");
            set_clipboard_text(owner.hwnd, "PASTED", true).unwrap();
            assert!(backup.restore_if_still_owned(owner.hwnd, "PASTED").unwrap());
            let actual = powershell("[Console]::Write((Get-Clipboard -Raw))");
            assert_eq!(actual, original);
            println!("text round {round}/8: restored original verified by another process");
        }

        // Persistent HTML from a separate process also exercises registered
        // HGLOBAL formats, without depending on the source process staying alive.
        powershell(
            "Add-Type -AssemblyName System.Windows.Forms; $data = New-Object System.Windows.Forms.DataObject; $data.SetData('HTML Format', '<html><body>ORIGINAL-HTML</body></html>'); [System.Windows.Forms.Clipboard]::SetDataObject($data, $true)",
        );
        let backup = ClipboardBackup::capture().unwrap().expect("HTML snapshot");
        set_clipboard_text(owner.hwnd, "PASTED", true).unwrap();
        assert!(backup.restore_if_still_owned(owner.hwnd, "PASTED").unwrap());
        assert_eq!(
            powershell(
                "Add-Type -AssemblyName System.Windows.Forms; [Console]::Write([System.Windows.Forms.Clipboard]::GetData('HTML Format'))"
            ),
            "<html><body>ORIGINAL-HTML</body></html>"
        );
        println!("HTML: restored original verified by another process");

        let backup = ClipboardBackup::capture().unwrap().unwrap();
        set_clipboard_text(owner.hwnd, "PASTED", true).unwrap();
        powershell("Set-Clipboard 'NEWER-CONTENTS'");
        assert!(!backup.restore_if_still_owned(owner.hwnd, "PASTED").unwrap());
        assert_eq!(
            powershell("[Console]::Write((Get-Clipboard -Raw))"),
            "NEWER-CONTENTS"
        );
        println!("newer contents: preserved");

        {
            let _clipboard = ClipboardOpen::open(Some(owner.hwnd)).unwrap();
            unsafe { EmptyClipboard().unwrap() };
        }
        let backup = ClipboardBackup::capture().unwrap().unwrap();
        assert!(backup.formats.is_empty());
        set_clipboard_text(owner.hwnd, "PASTED", true).unwrap();
        {
            let _clipboard = ClipboardOpen::open(None).unwrap();
            for name in [
                windows::core::w!("Clipboard Viewer Ignore"),
                windows::core::w!("ExcludeClipboardContentFromMonitorProcessing"),
                windows::core::w!("CanIncludeInClipboardHistory"),
                windows::core::w!("CanUploadToCloudClipboard"),
            ] {
                let format = register_clipboard_format(name).unwrap();
                let handle = HGLOBAL(unsafe { GetClipboardData(format).unwrap() }.0);
                let locked = unsafe { GlobalLock(handle) };
                assert!(!locked.is_null());
                assert_eq!(unsafe { *locked.cast::<u32>() }, 0);
                let _ = unsafe { GlobalUnlock(handle) };
            }
        }
        assert!(backup.restore_if_still_owned(owner.hwnd, "PASTED").unwrap());
        {
            let _clipboard = ClipboardOpen::open(None).unwrap();
            for name in [
                windows::core::w!("Clipboard Viewer Ignore"),
                windows::core::w!("ExcludeClipboardContentFromMonitorProcessing"),
            ] {
                let format = register_clipboard_format(name).unwrap();
                let handle = HGLOBAL(unsafe { GetClipboardData(format).unwrap() }.0);
                assert!(unsafe { GlobalSize(handle) } >= size_of::<u32>());
                let locked = unsafe { GlobalLock(handle) };
                assert!(!locked.is_null());
                assert_eq!(unsafe { *locked.cast::<u32>() }, 0);
                let _ = unsafe { GlobalUnlock(handle) };
            }
            for name in [
                windows::core::w!("CanIncludeInClipboardHistory"),
                windows::core::w!("CanUploadToCloudClipboard"),
            ] {
                assert!(
                    unsafe { GetClipboardData(register_clipboard_format(name).unwrap()) }.is_err()
                );
            }
        }
        println!("empty clipboard: restored with monitor markers; temporary markers verified");

        copy_text("FALLBACK-COPY").unwrap();
        {
            let _clipboard = ClipboardOpen::open(None).unwrap();
            assert!(clipboard_text_matches("FALLBACK-COPY"));
            for name in [
                windows::core::w!("Clipboard Viewer Ignore"),
                windows::core::w!("ExcludeClipboardContentFromMonitorProcessing"),
                windows::core::w!("CanIncludeInClipboardHistory"),
                windows::core::w!("CanUploadToCloudClipboard"),
            ] {
                assert!(
                    unsafe { GetClipboardData(register_clipboard_format(name).unwrap()) }.is_err()
                );
            }
        }
        println!("fallback copy: text retained, no ignore markers");
    }
}

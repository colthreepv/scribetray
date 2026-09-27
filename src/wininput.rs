//! Windows focus, caret, clipboard, and text input helpers.
//!
//! Clipboard restoration uses an `IDataObject` obtained with `OleGetClipboard`, so
//! formats exposed by the original clipboard object are retained where the source
//! data object supports them. The temporary paste payload itself is CF_UNICODETEXT.

use std::mem::{ManuallyDrop, size_of};
use std::ptr;
use std::slice;
use std::thread::sleep;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{info, warn};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HINSTANCE, HWND, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject, SAFEARRAY};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_UNICODETEXT, OleGetClipboard, OleInitialize, OleSetClipboard, OleUninitialize,
    SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Accessibility::{
    AccessibleObjectFromWindow, CUIAutomation, IAccessible, IUIAutomation, IUIAutomationElement,
    IUIAutomationTextPattern, IUIAutomationTextPattern2, IUIAutomationTextRange,
    UIA_TextPattern2Id, UIA_TextPatternId,
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
    #[error("clipboard data could not be read for preservation: {0}")]
    ClipboardSnapshot(WindowsError),
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

/// Return whether the saved target is still the active foreground/focus target.
///
/// If a RuntimeId was captured, failure to query the current RuntimeId is treated
/// as a failed guard. HWND comparison is used when the snapshot had no RuntimeId.
pub fn target_is_current(target: &TargetSnapshot) -> bool {
    let Ok(_com) = ComApartment::initialize() else {
        return false;
    };
    target_is_current_inner(target)
}

/// Insert text only if the captured focus is still current.
///
/// For paste mode, the prior clipboard data object is restored after a short
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
    set_clipboard_text(owner.hwnd, text)
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
        Some(ClipboardBackup::capture().map_err(WinInputError::ClipboardSnapshot)?)
    } else {
        None
    };

    let insert_result = set_clipboard_text(owner.hwnd, text).and_then(|()| {
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
    fn initialize() -> Result<Self> {
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
        unsafe { OpenClipboard(owner)? };
        Ok(Self)
    }
}

impl Drop for ClipboardOpen {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

enum ClipboardBackup {
    Empty,
    Data(IDataObject),
}

impl ClipboardBackup {
    fn capture() -> WindowsResult<Self> {
        let has_formats = {
            let _clipboard = ClipboardOpen::open(None)?;
            unsafe { EnumClipboardFormats(0) != 0 }
        };
        if !has_formats {
            return Ok(Self::Empty);
        }
        unsafe { OleGetClipboard() }.map(Self::Data)
    }

    fn restore(self, owner: HWND) -> WindowsResult<()> {
        match self {
            Self::Empty => {
                let _clipboard = ClipboardOpen::open(Some(owner))?;
                unsafe { EmptyClipboard() }
            }
            Self::Data(data) => unsafe { OleSetClipboard(Some(&data)) },
        }
    }

    fn restore_if_still_owned(
        self,
        temporary_owner: HWND,
        pasted_text: &str,
    ) -> WindowsResult<bool> {
        let still_contains_pasted_text = if unsafe { GetClipboardOwner()? } == temporary_owner {
            true
        } else {
            let mut matches = false;
            for attempt in 0..5 {
                if clipboard_text_matches(pasted_text) {
                    matches = true;
                    break;
                }
                if attempt < 4 {
                    sleep(Duration::from_millis(20));
                }
            }
            matches
        };
        if !still_contains_pasted_text {
            // Another application has replaced our temporary text with newer
            // clipboard contents; leave those contents intact.
            return Ok(false);
        }
        self.restore(temporary_owner)?;
        Ok(true)
    }
}

fn clipboard_text_matches(expected: &str) -> bool {
    let Ok(_clipboard) = ClipboardOpen::open(None) else {
        return false;
    };
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

fn set_clipboard_text(owner: HWND, text: &str) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = utf16
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or(WinInputError::ClipboardTextTooLarge)?;

    let mut memory = GlobalMemory {
        handle: unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes)? },
        transferred: false,
    };
    let locked = unsafe { GlobalLock(memory.handle) };
    if locked.is_null() {
        return Err(WinInputError::Windows(WindowsError::from_thread()));
    }
    unsafe {
        ptr::copy_nonoverlapping(utf16.as_ptr(), locked.cast::<u16>(), utf16.len());
        let _ = GlobalUnlock(memory.handle);
    }

    let _clipboard = ClipboardOpen::open(Some(owner))?;
    unsafe { EmptyClipboard()? };
    match unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.handle.0))) } {
        Ok(_) => {
            memory.transferred = true;
            Ok(())
        }
        Err(error) => Err(WinInputError::Windows(error)),
    }
}

struct GlobalMemory {
    handle: HGLOBAL,
    transferred: bool,
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

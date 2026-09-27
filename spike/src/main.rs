#[cfg(not(target_os = "windows"))]
compile_error!("scribetray-spike is a Windows-only CLI");

use std::error::Error;
use std::mem::{ManuallyDrop, size_of};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_UNICODETEXT, OleGetClipboard, OleSetClipboard, SafeArrayDestroy, SafeArrayGetDim,
    SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Accessibility::{
    AccessibleObjectFromWindow, CUIAutomation, IAccessible, IUIAutomation,
    IUIAutomationTextPattern2, UIA_TextPattern2Id,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, MOD_ALT, MOD_NOREPEAT, MOD_WIN, RegisterHotKey, SendInput, UnregisterHotKey,
    VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GUITHREADINFO, GetClassNameW, GetCursorPos,
    GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowTextW, GetWindowThreadProcessId,
    KBDLLHOOKSTRUCT, MSG, OBJID_CARET, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_HOTKEY,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};
use windows::core::{Interface, Result as WinResult};

const HOTKEY_ID: i32 = 0x5343;
const WM_CANCEL_CAPTURE: u32 = WM_APP + 1;
const MARKER: &str = "[ScribeTray]";
const MAX_CLIPBOARD_TEXT_BYTES: usize = 16 * 1024 * 1024;
const PASTE_SETTLE_DELAY: Duration = Duration::from_millis(250);

static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL_PENDING: AtomicBool = AtomicBool::new(false);
static OWNER_THREAD_ID: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug)]
struct ScreenRect {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

impl ScreenRect {
    fn is_valid(self) -> bool {
        self.left.is_finite()
            && self.top.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 0.0
            && self.height > 0.0
    }
}

#[derive(Clone, Copy)]
struct CaretReport {
    rect: ScreenRect,
    source: &'static str,
    method: &'static str,
}

enum UiaCaretResult {
    Rect(ScreenRect),
    Inactive,
    EmptyBoundingRectangles,
    UnsupportedBoundingRectangles,
}

struct Snapshot {
    foreground: HWND,
    focus: HWND,
    focus_runtime_id: Option<Vec<i32>>,
    caret: CaretReport,
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct HotkeyGuard;

impl Drop for HotkeyGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterHotKey(None, HOTKEY_ID);
        }
    }
}

struct KeyboardHook(windows::Win32::UI::WindowsAndMessaging::HHOOK);

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        CAPTURE_ACTIVE.store(false, Ordering::SeqCst);
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
    }
}

struct SafeArrayGuard(*mut windows::Win32::System::Com::SAFEARRAY);

impl Drop for SafeArrayGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = SafeArrayDestroy(self.0);
            }
        }
    }
}

struct ClipboardOpenGuard;

impl Drop for ClipboardOpenGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

struct GlobalUnlockGuard(HGLOBAL);

impl Drop for GlobalUnlockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.0);
        }
    }
}

struct ClipboardAllocation {
    handle: HGLOBAL,
    transferred: bool,
}

impl Drop for ClipboardAllocation {
    fn drop(&mut self) {
        if !self.transferred {
            unsafe {
                let _ = GlobalFree(Some(self.handle));
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ClipboardRestoration {
    OleDataObject,
    TextFallback,
    ClearedFallback,
    Failed,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let _com = ComApartment;

    // Force creation of this thread's message queue before the low-level hook can post to it.
    let mut queued_message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut queued_message, None, 0, 0, PM_NOREMOVE);
    }
    OWNER_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)? };
    let _hook = KeyboardHook(hook);

    unsafe {
        RegisterHotKey(
            None,
            HOTKEY_ID,
            MOD_WIN | MOD_ALT | MOD_NOREPEAT,
            VK_V.0 as u32,
        )?;
    }
    let _hotkey = HotkeyGuard;

    println!("ScribeTray M0 caret spike is running.");
    println!(
        "Win+Alt+V snapshots the foreground/focus/caret, then pastes {MARKER} on the next press."
    );
    println!("Escape cancels an active snapshot. Close this console to stop.");

    let mut capture: Option<Snapshot> = None;
    loop {
        let mut message = MSG::default();
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        if result.0 == 0 {
            break;
        }

        match message.message {
            WM_HOTKEY if message.wParam.0 as i32 == HOTKEY_ID => {
                if let Some(snapshot) = capture.take() {
                    CAPTURE_ACTIVE.store(false, Ordering::SeqCst);
                    CANCEL_PENDING.store(false, Ordering::SeqCst);
                    paste_with_focus_guard(snapshot);
                } else {
                    capture = Some(capture_now());
                    CANCEL_PENDING.store(false, Ordering::SeqCst);
                    CAPTURE_ACTIVE.store(true, Ordering::SeqCst);
                    println!("snapshot armed; press Win+Alt+V again to paste, or Escape to cancel");
                }
            }
            WM_CANCEL_CAPTURE => {
                CANCEL_PENDING.store(false, Ordering::SeqCst);
                if capture.take().is_some() {
                    CAPTURE_ACTIVE.store(false, Ordering::SeqCst);
                    println!("capture cancelled by Escape");
                }
            }
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }

        // A failed PostThreadMessageW still leaves cancellation observable on the next message.
        if CANCEL_PENDING.swap(false, Ordering::SeqCst) && capture.take().is_some() {
            CAPTURE_ACTIVE.store(false, Ordering::SeqCst);
            println!("capture cancelled by Escape");
        }
    }

    Ok(())
}

fn capture_now() -> Snapshot {
    let foreground = unsafe { GetForegroundWindow() };
    let info = gui_thread_info(foreground);
    let focus = info.map(|gui| gui.hwndFocus).unwrap_or_default();
    let focus_runtime_id = if focus.0.is_null() {
        eprintln!("focused element RuntimeId: unavailable because focused HWND is null");
        None
    } else {
        match focused_element_runtime_id() {
            Ok(Some(runtime_id)) => {
                println!("focused element RuntimeId: {runtime_id:?}");
                Some(runtime_id)
            }
            Ok(None) => {
                eprintln!("focused element RuntimeId: unavailable; HWND guard will be used");
                None
            }
            Err(error) => {
                eprintln!("focused element RuntimeId: {error:?}; HWND guard will be used");
                None
            }
        }
    };

    println!("foreground top-level: {}", describe_window(foreground));
    if info.is_some() {
        println!("focused HWND:          {}", describe_window(focus));
    } else {
        println!("focused HWND:          unavailable (GetGUIThreadInfo failed)");
    }

    let caret = resolve_caret(info);
    println!(
        "caret: source={} method={} rect=({}, {}, {:.1}, {:.1})",
        caret.source,
        caret.method,
        caret.rect.left,
        caret.rect.top,
        caret.rect.width,
        caret.rect.height
    );

    Snapshot {
        foreground,
        focus,
        focus_runtime_id,
        caret,
    }
}

fn gui_thread_info(foreground: HWND) -> Option<GUITHREADINFO> {
    if foreground.0.is_null() {
        return None;
    }
    let thread_id = unsafe { GetWindowThreadProcessId(foreground, None) };
    if thread_id == 0 {
        return None;
    }

    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(thread_id, &mut info).ok()? };
    Some(info)
}

fn resolve_caret(info: Option<GUITHREADINFO>) -> CaretReport {
    if let Some(gui) = info {
        if let Some(rect) = gui_caret_rect(gui.hwndCaret, gui.rcCaret) {
            return CaretReport {
                rect,
                source: "GetGUIThreadInfo",
                method: "rcCaret converted from client to screen coordinates",
            };
        }
        eprintln!("GetGUIThreadInfo: no usable system caret rectangle");
    } else {
        eprintln!("GetGUIThreadInfo: unable to read foreground thread GUI state");
    }

    match info.map(|gui| gui.hwndCaret) {
        Some(caret_window) if !caret_window.0.is_null() => match msaa_caret_rect(caret_window) {
            Ok(Some(rect)) => {
                return CaretReport {
                    rect,
                    source: "MSAA",
                    method: "AccessibleObjectFromWindow(hwndCaret, OBJID_CARET) + IAccessible::accLocation",
                };
            }
            Ok(None) => eprintln!("MSAA OBJID_CARET: no usable accessible rectangle"),
            Err(error) => eprintln!("MSAA OBJID_CARET: {error:?}"),
        },
        _ => eprintln!("MSAA OBJID_CARET: skipped because hwndCaret is null or unavailable"),
    }

    match uia_caret_rect() {
        Ok(UiaCaretResult::Rect(rect)) => {
            return CaretReport {
                rect,
                source: "UI Automation",
                method: "TextPattern2::GetCaretRange (zero-length range) + GetBoundingRectangles",
            };
        }
        Ok(UiaCaretResult::Inactive) => {
            eprintln!("UI Automation TextPattern2: GetCaretRange reported an inactive caret")
        }
        Ok(UiaCaretResult::EmptyBoundingRectangles) => eprintln!(
            "UI Automation TextPattern2: GetCaretRange returned a zero-length range, but this app provided no bounding rectangles"
        ),
        Ok(UiaCaretResult::UnsupportedBoundingRectangles) => eprintln!(
            "UI Automation TextPattern2: app returned bounding rectangles in an unsupported shape"
        ),
        Err(error) => eprintln!("UI Automation TextPattern2: {error:?}"),
    }

    let mut cursor = POINT::default();
    let rect = if unsafe { GetCursorPos(&mut cursor).is_ok() } {
        ScreenRect {
            left: cursor.x as f64,
            top: cursor.y as f64,
            width: 1.0,
            height: 1.0,
        }
    } else {
        ScreenRect {
            left: 0.0,
            top: 0.0,
            width: 1.0,
            height: 1.0,
        }
    };
    CaretReport {
        rect,
        source: "mouse fallback (not a caret)",
        method: "GetCursorPos",
    }
}

fn gui_caret_rect(caret_window: HWND, client_rect: RECT) -> Option<ScreenRect> {
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
    let rect = ScreenRect {
        left: top_left.x as f64,
        top: top_left.y as f64,
        width: (bottom_right.x - top_left.x) as f64,
        height: (bottom_right.y - top_left.y) as f64,
    };
    rect.is_valid().then_some(rect)
}

fn msaa_caret_rect(caret_window: HWND) -> WinResult<Option<ScreenRect>> {
    if caret_window.0.is_null() {
        return Ok(None);
    }
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
                    lVal: windows::Win32::UI::WindowsAndMessaging::CHILDID_SELF as i32,
                },
            }),
        },
    };

    let (mut left, mut top, mut width, mut height) = (0, 0, 0, 0);
    unsafe {
        accessible.accLocation(&mut left, &mut top, &mut width, &mut height, &child)?;
    }
    let rect = ScreenRect {
        left: left as f64,
        top: top as f64,
        width: width as f64,
        height: height as f64,
    };
    Ok(rect.is_valid().then_some(rect))
}

fn uia_caret_rect() -> WinResult<UiaCaretResult> {
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    let focused = unsafe { automation.GetFocusedElement()? };
    let text_pattern: IUIAutomationTextPattern2 =
        unsafe { focused.GetCurrentPatternAs(UIA_TextPattern2Id)? };
    let mut is_active = windows::core::BOOL(0);
    let range = unsafe { text_pattern.GetCaretRange(&mut is_active)? };
    if !is_active.as_bool() {
        return Ok(UiaCaretResult::Inactive);
    }

    let safe_array = unsafe { range.GetBoundingRectangles()? };
    if safe_array.is_null() {
        return Ok(UiaCaretResult::EmptyBoundingRectangles);
    }
    let safe_array = SafeArrayGuard(safe_array);
    if unsafe { SafeArrayGetDim(safe_array.0) } != 1 {
        return Ok(UiaCaretResult::UnsupportedBoundingRectangles);
    }
    let lower = unsafe { SafeArrayGetLBound(safe_array.0, 1)? };
    let upper = unsafe { SafeArrayGetUBound(safe_array.0, 1)? };
    if upper < lower {
        return Ok(UiaCaretResult::EmptyBoundingRectangles);
    }
    if upper - lower + 1 < 4 {
        return Ok(UiaCaretResult::UnsupportedBoundingRectangles);
    }

    let mut values = [0.0_f64; 4];
    for (offset, value) in values.iter_mut().enumerate() {
        let index = lower + offset as i32;
        unsafe {
            SafeArrayGetElement(safe_array.0, &index, (value as *mut f64).cast())?;
        }
    }
    let rect = ScreenRect {
        left: values[0],
        top: values[1],
        width: values[2],
        height: values[3],
    };
    if rect.is_valid() {
        Ok(UiaCaretResult::Rect(rect))
    } else {
        Ok(UiaCaretResult::UnsupportedBoundingRectangles)
    }
}

fn focused_element_runtime_id() -> WinResult<Option<Vec<i32>>> {
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    let focused = unsafe { automation.GetFocusedElement()? };
    let runtime_array = unsafe { focused.GetRuntimeId()? };
    if runtime_array.is_null() {
        return Ok(None);
    }

    let runtime_array = SafeArrayGuard(runtime_array);
    if unsafe { SafeArrayGetDim(runtime_array.0) } != 1 {
        return Ok(None);
    }
    let lower = unsafe { SafeArrayGetLBound(runtime_array.0, 1)? };
    let upper = unsafe { SafeArrayGetUBound(runtime_array.0, 1)? };
    let component_count = i64::from(upper) - i64::from(lower) + 1;
    if !(1..=64).contains(&component_count) {
        return Ok(None);
    }

    let mut runtime_id = Vec::with_capacity(component_count as usize);
    for offset in 0..component_count {
        let index = lower + offset as i32;
        let mut component = 0_i32;
        unsafe {
            SafeArrayGetElement(runtime_array.0, &index, (&mut component as *mut i32).cast())?;
        }
        runtime_id.push(component);
    }
    Ok(Some(runtime_id))
}

fn focus_guard_matches(snapshot: &Snapshot) -> bool {
    let current_foreground = unsafe { GetForegroundWindow() };
    let current_focus = gui_thread_info(current_foreground)
        .map(|info| info.hwndFocus)
        .unwrap_or_default();

    if snapshot.foreground.0.is_null()
        || snapshot.focus.0.is_null()
        || current_foreground != snapshot.foreground
        || current_focus != snapshot.focus
    {
        eprintln!("paste refused: foreground/focused HWND changed since the snapshot");
        eprintln!(
            "snapshot foreground: {}",
            describe_window(snapshot.foreground)
        );
        eprintln!(
            "current foreground:  {}",
            describe_window(current_foreground)
        );
        eprintln!("snapshot focus:      {}", describe_window(snapshot.focus));
        eprintln!("current focus:       {}", describe_window(current_focus));
        return false;
    }

    let current_runtime_id = match focused_element_runtime_id() {
        Ok(runtime_id) => runtime_id,
        Err(error) => {
            eprintln!("focused element RuntimeId unavailable; using HWND fallback: {error:?}");
            None
        }
    };
    match (&snapshot.focus_runtime_id, current_runtime_id) {
        (Some(saved), Some(current)) if saved != &current => {
            eprintln!("paste refused: focused element RuntimeId changed");
            eprintln!("snapshot RuntimeId: {saved:?}");
            eprintln!("current RuntimeId:  {current:?}");
            return false;
        }
        (Some(_), Some(_)) => println!("focus guard: focused element RuntimeId matched"),
        _ => println!("focus guard: UIA RuntimeId unavailable; foreground/focused HWND matched"),
    }
    true
}

fn paste_with_focus_guard(snapshot: Snapshot) {
    if !wait_for_hotkey_release() {
        eprintln!("paste refused: hotkey chord was still held after 1 second");
        return;
    }

    let clipboard_owner = unsafe { GetConsoleWindow() };
    if clipboard_owner.0.is_null() {
        eprintln!("paste refused: no console window is available to own the clipboard");
        return;
    }
    let previous_object = match unsafe { OleGetClipboard() } {
        Ok(data_object) => data_object,
        Err(error) => {
            eprintln!("paste refused: OleGetClipboard could not snapshot the clipboard: {error:?}");
            return;
        }
    };
    let previous_text = match read_clipboard_text(clipboard_owner) {
        Ok(text) => text,
        Err(error) => {
            eprintln!(
                "clipboard text snapshot unavailable; retaining the OLE data object: {error:?}"
            );
            None
        }
    };

    if !focus_guard_matches(&snapshot) {
        return;
    }

    if let Err(error) = write_clipboard_text(clipboard_owner, MARKER) {
        eprintln!("paste refused: could not set marker text on the clipboard: {error:?}");
        let restoration =
            restore_clipboard(clipboard_owner, &previous_object, previous_text.as_deref());
        eprintln!("clipboard restoration result: {restoration:?}");
        return;
    }

    let expected = 4;
    let sent = send_ctrl_v();
    if sent > 0 {
        // Give the foreground app time to consume the temporary clipboard contents.
        sleep(PASTE_SETTLE_DELAY);
    }
    let restoration =
        restore_clipboard(clipboard_owner, &previous_object, previous_text.as_deref());

    if sent == expected {
        println!(
            "pasted {MARKER}; clipboard restoration={restoration:?}; snapshot caret source={} rect=({}, {}, {:.1}, {:.1}); no Enter key was sent",
            snapshot.caret.source,
            snapshot.caret.rect.left,
            snapshot.caret.rect.top,
            snapshot.caret.rect.width,
            snapshot.caret.rect.height
        );
    } else {
        eprintln!("SendInput sent {sent}/{expected} Ctrl+V inputs; paste may not have occurred");
        eprintln!("clipboard restoration result: {restoration:?}");
    }
}

fn read_clipboard_text(owner: HWND) -> WinResult<Option<String>> {
    unsafe { OpenClipboard(Some(owner))? };
    let _clipboard_open = ClipboardOpenGuard;
    let format = CF_UNICODETEXT.0 as u32;
    if unsafe { IsClipboardFormatAvailable(format) }.is_err() {
        return Ok(None);
    }
    let handle = unsafe { GetClipboardData(format)? };
    if handle.0.is_null() {
        return Ok(None);
    }
    let memory = HGLOBAL(handle.0);
    let byte_count = unsafe { GlobalSize(memory) };
    if byte_count < size_of::<u16>() || byte_count > MAX_CLIPBOARD_TEXT_BYTES {
        return Ok(None);
    }
    let locked = unsafe { GlobalLock(memory) };
    if locked.is_null() {
        return Err(windows::core::Error::from_thread());
    }
    let _global_locked = GlobalUnlockGuard(memory);
    let units =
        unsafe { std::slice::from_raw_parts(locked.cast::<u16>(), byte_count / size_of::<u16>()) };
    let length = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    Ok(Some(String::from_utf16_lossy(&units[..length])))
}

fn write_clipboard_text(owner: HWND, text: &str) -> WinResult<()> {
    unsafe { OpenClipboard(Some(owner))? };
    let _clipboard_open = ClipboardOpenGuard;

    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, utf16.len() * size_of::<u16>())? };
    let mut allocation = ClipboardAllocation {
        handle: memory,
        transferred: false,
    };
    let locked = unsafe { GlobalLock(memory) };
    if locked.is_null() {
        return Err(windows::core::Error::from_thread());
    }
    unsafe {
        ptr::copy_nonoverlapping(utf16.as_ptr(), locked.cast::<u16>(), utf16.len());
    }
    drop(GlobalUnlockGuard(memory));

    unsafe { EmptyClipboard()? };
    unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.0)))? };
    allocation.transferred = true;
    Ok(())
}

fn clear_clipboard(owner: HWND) -> WinResult<()> {
    unsafe { OpenClipboard(Some(owner))? };
    let _clipboard_open = ClipboardOpenGuard;
    unsafe { EmptyClipboard() }
}

fn restore_clipboard(
    owner: HWND,
    previous_object: &windows::Win32::System::Com::IDataObject,
    previous_text: Option<&str>,
) -> ClipboardRestoration {
    match unsafe { OleSetClipboard(previous_object) } {
        Ok(()) => ClipboardRestoration::OleDataObject,
        Err(error) => {
            eprintln!("OleSetClipboard restoration failed: {error:?}");
            match previous_text {
                Some(text) => match write_clipboard_text(owner, text) {
                    Ok(()) => ClipboardRestoration::TextFallback,
                    Err(error) => {
                        eprintln!("plain-text clipboard fallback failed: {error:?}");
                        ClipboardRestoration::Failed
                    }
                },
                None => match clear_clipboard(owner) {
                    Ok(()) => ClipboardRestoration::ClearedFallback,
                    Err(error) => {
                        eprintln!("clipboard clear fallback failed: {error:?}");
                        ClipboardRestoration::Failed
                    }
                },
            }
        }
    }
}

fn send_ctrl_v() -> u32 {
    let inputs = [
        virtual_key_input(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        virtual_key_input(VK_V, KEYBD_EVENT_FLAGS(0)),
        virtual_key_input(VK_V, KEYEVENTF_KEYUP),
        virtual_key_input(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent < inputs.len() as u32 {
        let releases = [
            virtual_key_input(VK_V, KEYEVENTF_KEYUP),
            virtual_key_input(VK_CONTROL, KEYEVENTF_KEYUP),
        ];
        unsafe {
            SendInput(&releases, size_of::<INPUT>() as i32);
        }
    }
    sent
}

fn virtual_key_input(
    key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
    flags: KEYBD_EVENT_FLAGS,
) -> INPUT {
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

fn wait_for_hotkey_release() -> bool {
    let keys = [VK_V, VK_MENU, VK_LWIN, VK_RWIN];
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let any_down = keys
            .iter()
            .any(|key| unsafe { GetAsyncKeyState(key.0 as i32) } < 0);
        if !any_down {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(8));
    }
}

fn describe_window(hwnd: HWND) -> String {
    if hwnd.0.is_null() {
        return "HWND=0".to_owned();
    }
    let mut title = [0_u16; 256];
    let mut class_name = [0_u16; 128];
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_name) }.max(0) as usize;
    format!(
        "HWND=0x{:X} title={:?} class={:?}",
        hwnd.0 as usize,
        String::from_utf16_lossy(&title[..title_len]),
        String::from_utf16_lossy(&class_name[..class_len]),
    )
}

unsafe extern "system" fn keyboard_hook(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    if code >= 0
        && CAPTURE_ACTIVE.load(Ordering::SeqCst)
        && matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN)
    {
        let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if key.vkCode == VK_ESCAPE.0 as u32 {
            CANCEL_PENDING.store(true, Ordering::SeqCst);
            let thread_id = OWNER_THREAD_ID.load(Ordering::SeqCst);
            if thread_id != 0 {
                unsafe {
                    let _ = PostThreadMessageW(thread_id, WM_CANCEL_CAPTURE, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

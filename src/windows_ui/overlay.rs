use super::overlay_paint::{draw_overlay, present_layered_window};
use super::state::UiState;
use super::types::{AnchorStatus, CaretRect};
use super::{
    HOTKEY_CANCEL_ID, OVERLAY_ANIMATION_MS, OVERLAY_METER_MS, OVERLAY_RECORDING_TICK_MS,
    TIMER_OVERLAY_ANIMATION, TIMER_OVERLAY_METER, TIMER_RECORDING, TITLE,
};
use std::cell::Cell;
use std::mem::size_of;
use std::time::Instant;
use tiny_skia::Pixmap;
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetCursorPos, GetWindowLongPtrW, GetWindowRect, HTCLIENT, HTTRANSPARENT,
    HWND_TOPMOST, KillTimer, MA_NOACTIVATE, PostMessageW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, WM_CANCELMODE, WM_CAPTURECHANGED, WM_HOTKEY, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WS_EX_TRANSPARENT,
};

/// Caret overlay geometry in device-independent pixels, scaled per monitor.
pub(super) const PILL_WIDTH_DIP: f32 = 58.0;
pub(super) const PILL_COUNTDOWN_WIDTH_DIP: f32 = 90.0;
pub(super) const PILL_HEIGHT_DIP: f32 = 22.0;
pub(super) const PILL_RADIUS_DIP: f32 = 11.0;
/// Transparent margin around the pill that carries the soft shadow.
pub(super) const OVERLAY_SHADOW_PAD_DIP: f32 = 8.0;
const OVERLAY_GAP_DIP: f32 = 8.0;
const OVERLAY_CARET_INSET_DIP: f32 = 4.0;
pub(super) const OVERLAY_WAVE_BARS: usize = 7;
pub(super) const OVERLAY_BAR_WIDTH_DIP: f32 = 2.0;
pub(super) const OVERLAY_BAR_PITCH_DIP: f32 = 4.0;
pub(super) const OVERLAY_BAR_START_DIP: f32 = 21.0;
pub(super) const OVERLAY_BAR_MIN_DIP: f32 = 2.0;
pub(super) const OVERLAY_BAR_MAX_DIP: f32 = 14.0;
pub(super) const OVERLAY_DOT_X_DIP: f32 = 11.0;
pub(super) const OVERLAY_DOT_RADIUS_DIP: f32 = 3.0;
/// Per-frame easing applied to each waveform bar toward its sampled level.
const OVERLAY_BAR_EASING: f32 = 0.45;
pub(super) const OVERLAY_FADE_IN_SECONDS: f32 = 0.12;
pub(super) const OVERLAY_ERROR_HOLD_SECONDS: f32 = 2.0;
pub(super) const OVERLAY_ERROR_FADE_SECONDS: f32 = 0.2;
const DEFAULT_DPI: f32 = 96.0;
pub(super) const OVERLAY_CLOSE_SIZE_DIP: f32 = 18.0;

// There is one overlay per UI thread. Keep its mouse gesture local to that
// thread, just like the keyboard hook's interaction state.
thread_local! {
    static CLOSE_PRESSED: Cell<bool> = const { Cell::new(false) };
}

impl UiState {
    fn overlay_can_cancel(&self) -> bool {
        self.overlay_visible
            && self.recording
            && matches!(self.anchor_status, Some(AnchorStatus::Recording { .. }))
    }

    pub(super) fn overlay_close_visible(&self) -> bool {
        self.overlay_can_cancel()
            && unsafe { GetWindowLongPtrW(self.overlay_hwnd, GWL_EXSTYLE) }
                & WS_EX_TRANSPARENT.0 as isize
                == 0
    }

    fn set_overlay_hovered(&self, hovered: bool) {
        unsafe {
            let style = GetWindowLongPtrW(self.overlay_hwnd, GWL_EXSTYLE);
            let transparent = WS_EX_TRANSPARENT.0 as isize;
            let updated = if hovered {
                style & !transparent
            } else {
                style | transparent
            };
            if updated != style {
                SetWindowLongPtrW(self.overlay_hwnd, GWL_EXSTYLE, updated);
            }
            if !hovered {
                CLOSE_PRESSED.set(false);
                if GetCapture() == self.overlay_hwnd {
                    let _ = ReleaseCapture();
                }
            }
        }
    }

    fn update_overlay_hover(&self) {
        let mut cursor = POINT::default();
        let mut rect = RECT::default();
        let hovered = self.overlay_can_cancel()
            && unsafe { GetCursorPos(&mut cursor) }.is_ok()
            && unsafe { GetWindowRect(self.overlay_hwnd, &mut rect) }.is_ok()
            && point_in_rect(cursor, rect);
        self.set_overlay_hovered(hovered);
    }

    pub(super) fn overlay_mouse_message(
        &mut self,
        message: u32,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        match message {
            WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_NCHITTEST => {
                let mut rect = RECT::default();
                let point = message_point(lparam);
                let on_close = self.overlay_close_visible()
                    && unsafe { GetWindowRect(self.overlay_hwnd, &mut rect) }.is_ok()
                    && point_in_rect(
                        POINT {
                            x: point.x - rect.left,
                            y: point.y - rect.top,
                        },
                        overlay_close_rect(self.overlay_dpi_scale.max(0.5)),
                    );
                Some(LRESULT(if on_close {
                    HTCLIENT as isize
                } else {
                    HTTRANSPARENT as isize
                }))
            }
            WM_LBUTTONDOWN => {
                if self.overlay_close_visible()
                    && point_in_rect(
                        message_point(lparam),
                        overlay_close_rect(self.overlay_dpi_scale.max(0.5)),
                    )
                {
                    CLOSE_PRESSED.set(true);
                    unsafe {
                        SetCapture(self.overlay_hwnd);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let cancel = CLOSE_PRESSED.replace(false)
                    && self.overlay_close_visible()
                    && point_in_rect(
                        message_point(lparam),
                        overlay_close_rect(self.overlay_dpi_scale.max(0.5)),
                    );
                unsafe {
                    if GetCapture() == self.overlay_hwnd {
                        let _ = ReleaseCapture();
                    }
                    if cancel {
                        // Reuse the hotkey handler, including its hook cleanup.
                        let _ = PostMessageW(
                            Some(self.tray_hwnd),
                            WM_HOTKEY,
                            WPARAM(HOTKEY_CANCEL_ID as usize),
                            LPARAM(0),
                        );
                    }
                }
                Some(LRESULT(0))
            }
            WM_CANCELMODE | WM_CAPTURECHANGED => {
                CLOSE_PRESSED.set(false);
                if message == WM_CANCELMODE && unsafe { GetCapture() } == self.overlay_hwnd {
                    let _ = unsafe { ReleaseCapture() };
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    pub(super) fn show_anchor(&mut self, rect: CaretRect, status: AnchorStatus) {
        if !rect.is_valid() {
            self.hide_anchor();
            return;
        }
        // Restart the entry animation only when the overlay appears or changes
        // kind, so the ~200 ms caret poll does not make the pill blink.
        let kind_changed = self.anchor_status.map(overlay_kind) != Some(overlay_kind(status));
        let appearing = !self.overlay_visible;
        if kind_changed {
            self.overlay_state_since = Instant::now();
            self.overlay_levels = [0.0; OVERLAY_WAVE_BARS];
            self.overlay_shown = [0.0; OVERLAY_WAVE_BARS];
        }
        if appearing || kind_changed {
            self.overlay_shown_at = Instant::now();
        }
        self.anchor_rect = Some(rect);
        self.anchor_status = Some(status);
        if matches!(status, AnchorStatus::Error) {
            self.tray_error = true;
        }
        self.recording_started = match status {
            AnchorStatus::Recording { elapsed } => Instant::now().checked_sub(elapsed),
            _ => None,
        };
        self.overlay_dpi_scale = overlay_dpi_scale(rect);
        self.overlay_visible = true;
        self.reposition_overlay();
        self.update_overlay_hover();
        unsafe {
            let _ = ShowWindow(self.overlay_hwnd, SW_SHOWNOACTIVATE);
            if SetTimer(
                Some(self.overlay_hwnd),
                TIMER_OVERLAY_ANIMATION,
                OVERLAY_ANIMATION_MS,
                None,
            ) == 0
            {
                self.show_notice(TITLE, "The overlay animation timer could not be started.");
            }
            let _ = SetTimer(
                Some(self.overlay_hwnd),
                TIMER_OVERLAY_METER,
                OVERLAY_METER_MS,
                None,
            );
            if self.recording_started.is_some() {
                if SetTimer(
                    Some(self.overlay_hwnd),
                    TIMER_RECORDING,
                    OVERLAY_RECORDING_TICK_MS,
                    None,
                ) == 0
                {
                    self.show_notice(TITLE, "The recording timer could not be started.");
                }
            } else {
                let _ = KillTimer(Some(self.overlay_hwnd), TIMER_RECORDING);
            }
        }
        self.render_overlay();
    }

    pub(super) fn hide_anchor(&mut self) {
        self.anchor_rect = None;
        self.anchor_status = None;
        self.recording_started = None;
        self.overlay_visible = false;
        self.set_overlay_hovered(false);
        self.overlay_levels = [0.0; OVERLAY_WAVE_BARS];
        self.overlay_shown = [0.0; OVERLAY_WAVE_BARS];
        unsafe {
            let _ = KillTimer(Some(self.overlay_hwnd), TIMER_OVERLAY_ANIMATION);
            let _ = KillTimer(Some(self.overlay_hwnd), TIMER_OVERLAY_METER);
            let _ = KillTimer(Some(self.overlay_hwnd), TIMER_RECORDING);
            let _ = ShowWindow(self.overlay_hwnd, SW_HIDE);
        }
    }

    /// Re-applies the DPI-aware screen position and size of the overlay window.
    pub(super) fn reposition_overlay(&mut self) {
        let Some(rect) = self.anchor_rect else {
            return;
        };
        let scale = self.overlay_dpi_scale.max(0.5);
        let expanded = overlay_countdown(self).is_some();
        let (left, top) = overlay_window_position(rect, scale, expanded);
        unsafe {
            let _ = SetWindowPos(
                self.overlay_hwnd,
                Some(HWND_TOPMOST),
                left,
                top,
                overlay_window_width(scale, expanded),
                overlay_window_height(scale),
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            );
        }
    }

    /// Eases every waveform bar toward its sampled target and repaints.
    pub(super) fn advance_overlay_animation(&mut self) {
        if !self.overlay_visible {
            return;
        }
        self.update_overlay_hover();
        if matches!(self.anchor_status, Some(AnchorStatus::Error))
            && self.overlay_state_since.elapsed().as_secs_f32()
                >= OVERLAY_ERROR_HOLD_SECONDS + OVERLAY_ERROR_FADE_SECONDS
        {
            self.hide_anchor();
            self.refresh_tray();
            return;
        }
        for index in 0..OVERLAY_WAVE_BARS {
            let shown = self.overlay_shown[index];
            self.overlay_shown[index] =
                shown + (self.overlay_levels[index] - shown) * OVERLAY_BAR_EASING;
        }
        self.render_overlay();
    }

    /// Scrolls one fresh level from the recorder into the waveform history.
    pub(super) fn sample_overlay_meter(&mut self) {
        if !self.overlay_visible {
            return;
        }
        let level = self
            .level_meter
            .as_ref()
            .map(|meter| meter.level())
            .unwrap_or(0.0);
        self.overlay_levels.rotate_left(1);
        self.overlay_levels[OVERLAY_WAVE_BARS - 1] = level;
    }

    /// Rasterizes the current state and presents it as a premultiplied layer.
    pub(super) fn render_overlay(&mut self) {
        if !self.overlay_visible {
            return;
        }
        let scale = self.overlay_dpi_scale.max(0.5);
        let width = overlay_window_width(scale, overlay_countdown(self).is_some()).max(1) as u32;
        let height = overlay_window_height(scale).max(1) as u32;
        let Some(mut pixmap) = Pixmap::new(width, height) else {
            return;
        };
        draw_overlay(&mut pixmap, self, scale);
        if let Err(error) = present_layered_window(self.overlay_hwnd, &pixmap) {
            // Presentation can fail transiently during display changes; keep the
            // last good frame instead of tearing the overlay down.
            let _ = error;
        }
    }
}

fn message_point(lparam: LPARAM) -> POINT {
    POINT {
        x: lparam.0 as u16 as i16 as i32,
        y: (lparam.0 >> 16) as u16 as i16 as i32,
    }
}

fn point_in_rect(point: POINT, rect: RECT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

pub(super) fn overlay_close_rect(scale: f32) -> RECT {
    let left = OVERLAY_SHADOW_PAD_DIP + OVERLAY_DOT_X_DIP - OVERLAY_CLOSE_SIZE_DIP / 2.0;
    let top = OVERLAY_SHADOW_PAD_DIP + (PILL_HEIGHT_DIP - OVERLAY_CLOSE_SIZE_DIP) / 2.0;
    RECT {
        left: (left * scale).round() as i32,
        top: (top * scale).round() as i32,
        right: ((left + OVERLAY_CLOSE_SIZE_DIP) * scale).round() as i32,
        bottom: ((top + OVERLAY_CLOSE_SIZE_DIP) * scale).round() as i32,
    }
}

/// Coarse overlay shape used to detect state changes without comparing timing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OverlayKind {
    Recording,
    Working,
    Error,
}

pub(super) fn overlay_kind(status: AnchorStatus) -> OverlayKind {
    match status {
        AnchorStatus::Recording { .. } => OverlayKind::Recording,
        AnchorStatus::Working => OverlayKind::Working,
        AnchorStatus::Error => OverlayKind::Error,
    }
}

pub(super) fn overlay_countdown(state: &UiState) -> Option<u32> {
    let Some(AnchorStatus::Recording { elapsed }) = state.anchor_status else {
        return None;
    };
    let elapsed = elapsed.as_secs().min(u32::MAX as u64) as u32;
    let remaining = state.settings.max_seconds.saturating_sub(elapsed);
    (remaining <= 60).then_some(remaining)
}

/// Monitor that contains the caret, falling back to the nearest one.
fn caret_monitor(caret: CaretRect) -> Option<windows::Win32::Graphics::Gdi::HMONITOR> {
    let rect = RECT {
        left: caret.left,
        top: caret.top,
        right: caret.left.saturating_add(caret.width.max(1)),
        bottom: caret.top.saturating_add(caret.height.max(1)),
    };
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
    (!monitor.0.is_null()).then_some(monitor)
}

/// DPI scale factor (1.0 at 96 DPI) of the monitor that holds the caret.
pub(super) fn overlay_dpi_scale(caret: CaretRect) -> f32 {
    let Some(monitor) = caret_monitor(caret) else {
        return 1.0;
    };
    let mut dpi_x = DEFAULT_DPI as u32;
    let mut dpi_y = DEFAULT_DPI as u32;
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_ok()
        && dpi_x > 0
    {
        dpi_x as f32 / DEFAULT_DPI
    } else {
        1.0
    }
}

/// Work area (taskbar-excluded) of the monitor that holds the caret.
fn caret_work_area(caret: CaretRect) -> Option<RECT> {
    let monitor = caret_monitor(caret)?;
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        Some(info.rcWork)
    } else {
        None
    }
}

pub(super) fn overlay_window_width(scale: f32, expanded: bool) -> i32 {
    let pill_width = if expanded {
        PILL_COUNTDOWN_WIDTH_DIP
    } else {
        PILL_WIDTH_DIP
    };
    ((pill_width + OVERLAY_SHADOW_PAD_DIP * 2.0) * scale)
        .round()
        .max(1.0) as i32
}

pub(super) fn overlay_window_height(scale: f32) -> i32 {
    ((PILL_HEIGHT_DIP + OVERLAY_SHADOW_PAD_DIP * 2.0) * scale)
        .round()
        .max(1.0) as i32
}

/// Places the pill above the caret, flipping below and clamping into the work
/// area when the caret sits close to a screen edge.
fn overlay_window_position(caret: CaretRect, scale: f32, expanded: bool) -> (i32, i32) {
    let window_width = overlay_window_width(scale, expanded);
    let window_height = overlay_window_height(scale);
    let pad = (OVERLAY_SHADOW_PAD_DIP * scale).round() as i32;
    let gap = (OVERLAY_GAP_DIP * scale).round() as i32;
    let inset = (OVERLAY_CARET_INSET_DIP * scale).round() as i32;
    let pill_height = (PILL_HEIGHT_DIP * scale).round() as i32;
    let caret_bottom = caret.top.saturating_add(caret.height.max(1));

    let above = caret
        .top
        .saturating_sub(gap)
        .saturating_sub(pill_height)
        .saturating_sub(pad);
    let below = caret_bottom.saturating_add(gap).saturating_sub(pad);
    let mut left = caret.left.saturating_sub(inset).saturating_sub(pad);
    let mut top = above;

    if let Some(work) = caret_work_area(caret) {
        if top < work.top {
            top = below;
        }
        let max_left = work.right.saturating_sub(window_width).max(work.left);
        let max_top = work.bottom.saturating_sub(window_height).max(work.top);
        left = left.clamp(work.left, max_left);
        top = top.clamp(work.top, max_top);
    }
    (left, top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_hit_target_scales_and_stays_clear_of_waveform() {
        for scale in [0.5, 1.0, 1.25, 1.5, 2.0, 3.0] {
            let rect = overlay_close_rect(scale);
            let center = POINT {
                x: ((OVERLAY_SHADOW_PAD_DIP + OVERLAY_DOT_X_DIP) * scale).round() as i32,
                y: ((OVERLAY_SHADOW_PAD_DIP + PILL_HEIGHT_DIP / 2.0) * scale).round() as i32,
            };
            assert!(point_in_rect(center, rect));
            assert!(point_in_rect(
                POINT {
                    x: rect.left,
                    y: rect.top
                },
                rect
            ));
            assert!(!point_in_rect(
                POINT {
                    x: rect.right,
                    y: center.y
                },
                rect
            ));
            assert!(!point_in_rect(
                POINT {
                    x: center.x,
                    y: rect.bottom
                },
                rect
            ));
            assert!(!point_in_rect(
                POINT {
                    x: rect.left - 1,
                    y: center.y
                },
                rect
            ));
            assert!(!point_in_rect(
                POINT {
                    x: center.x,
                    y: rect.top - 1
                },
                rect
            ));
            assert!(
                rect.right
                    <= ((OVERLAY_SHADOW_PAD_DIP + OVERLAY_BAR_START_DIP) * scale).round() as i32
            );
        }
    }

    #[test]
    fn mouse_coordinates_preserve_negative_monitor_positions() {
        let packed = ((-200i16 as u16 as usize) << 16) | (-1200i16 as u16 as usize);
        let point = message_point(LPARAM(packed as isize));
        assert_eq!((point.x, point.y), (-1200, -200));
        assert!(point_in_rect(
            point,
            RECT {
                left: -1300,
                top: -250,
                right: -1100,
                bottom: -150
            }
        ));
    }
}

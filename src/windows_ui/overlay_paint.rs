use super::overlay::{
    OVERLAY_BAR_MAX_DIP, OVERLAY_BAR_MIN_DIP, OVERLAY_BAR_PITCH_DIP, OVERLAY_BAR_START_DIP,
    OVERLAY_BAR_WIDTH_DIP, OVERLAY_DOT_RADIUS_DIP, OVERLAY_DOT_X_DIP, OVERLAY_ERROR_FADE_SECONDS,
    OVERLAY_ERROR_HOLD_SECONDS, OVERLAY_FADE_IN_SECONDS, OVERLAY_SHADOW_PAD_DIP, OVERLAY_WAVE_BARS,
    OverlayKind, PILL_COUNTDOWN_WIDTH_DIP, PILL_HEIGHT_DIP, PILL_RADIUS_DIP, PILL_WIDTH_DIP,
    overlay_countdown, overlay_kind,
};
use super::state::UiState;
use super::types::AnchorStatus;
use super::wide;
use std::mem::size_of;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DeleteDC, DeleteObject, FF_DONTCARE,
    FW_SEMIBOLD, GetDC, GetTextExtentPoint32W, GetTextFaceW, GetTextMetricsW, HDC, HFONT, HGDIOBJ,
    OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT,
    TextOutW,
};
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};
use windows::core::PCWSTR;

/// Colors shared by the tray, recording overlay, and menu status states.
const REC_RGB: (u8, u8, u8) = (0xF0, 0x44, 0x38);
const WORK_RGB: (u8, u8, u8) = (0xF5, 0xA5, 0x24);
const BAR_RGB: (u8, u8, u8) = (0xF0, 0xF2, 0xF5);
const PILL_FILL_RGB: (u8, u8, u8) = (22, 24, 29);
const PILL_BORDER_RGB: (u8, u8, u8) = (255, 255, 255);
/// Cubic handle used to approximate circular corners and dots.
const CIRCLE_KAPPA: f32 = 0.552_284_75;

/// Rasterizes the current overlay state into a premultiplied RGBA pixmap.
///
/// The geometry is written in device-independent pixels and multiplied by the
/// monitor scale factor.
pub(super) fn draw_overlay(pixmap: &mut Pixmap, state: &UiState, scale: f32) {
    let pad = OVERLAY_SHADOW_PAD_DIP * scale;
    let countdown = overlay_countdown(state);
    let pill_width = if countdown.is_some() {
        PILL_COUNTDOWN_WIDTH_DIP * scale
    } else {
        PILL_WIDTH_DIP * scale
    };
    let pill_height = PILL_HEIGHT_DIP * scale;
    let radius = PILL_RADIUS_DIP * scale;
    let center_y = pad + pill_height / 2.0;

    let since_show = state.overlay_shown_at.elapsed().as_secs_f32();
    let mut alpha = (since_show / OVERLAY_FADE_IN_SECONDS).clamp(0.0, 1.0);
    let kind = state.anchor_status.map(overlay_kind);
    let state_seconds = state.overlay_state_since.elapsed().as_secs_f32();
    if kind == Some(OverlayKind::Error) {
        alpha *= error_fade(state_seconds);
    }
    alpha = alpha.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }

    let shadow_offset = 2.0 * scale;
    if kind == Some(OverlayKind::Error) {
        // Error collapses to a 22x22 circle with a red cross.
        let compact_radius = pill_height / 2.0;
        let center_x = pad + compact_radius;
        for step in 1..=3 {
            let grow = step as f32 * 2.5 * scale;
            if let Some(path) =
                circle_path(center_x, center_y + shadow_offset, compact_radius + grow)
            {
                fill_path(pixmap, &path, rgba8(0, 0, 0, (26.0 * alpha) as u8));
            }
        }
        if let Some(path) = circle_path(center_x, center_y, compact_radius) {
            fill_path(pixmap, &path, pill_fill(240.0 * alpha));
        }
        if let Some(path) = circle_path(center_x, center_y, compact_radius - 0.5 * scale) {
            stroke_path(pixmap, &path, pill_border(alpha), scale);
        }
        let reach = 3.5 * scale;
        let color = record_color(alpha);
        if let Some(path) = line_path(
            center_x - reach,
            center_y - reach,
            center_x + reach,
            center_y + reach,
        ) {
            stroke_path(pixmap, &path, color, 2.0 * scale);
        }
        if let Some(path) = line_path(
            center_x - reach,
            center_y + reach,
            center_x + reach,
            center_y - reach,
        ) {
            stroke_path(pixmap, &path, color, 2.0 * scale);
        }
        return;
    }

    // Soft shadow: nested translucent rounded rectangles stand in for the 8 px
    // blur, which keeps the 33 ms animation cheap.
    for step in 1..=3 {
        let grow = step as f32 * 2.5 * scale;
        if let Some(path) = round_rect_path(
            pad - grow,
            pad - grow + shadow_offset,
            pill_width + grow * 2.0,
            pill_height + grow * 2.0,
            radius + grow,
        ) {
            fill_path(pixmap, &path, rgba8(0, 0, 0, (26.0 * alpha) as u8));
        }
    }
    if let Some(path) = round_rect_path(pad, pad, pill_width, pill_height, radius) {
        fill_path(pixmap, &path, pill_fill(240.0 * alpha));
    }
    if let Some(path) = round_rect_path(
        pad + 0.5 * scale,
        pad + 0.5 * scale,
        (pill_width - scale).max(0.0),
        (pill_height - scale).max(0.0),
        (radius - 0.5 * scale).max(0.0),
    ) {
        stroke_path(pixmap, &path, pill_border(alpha), scale);
    }

    match kind {
        Some(OverlayKind::Recording) => {
            let elapsed = match state.anchor_status {
                Some(AnchorStatus::Recording { elapsed }) => elapsed.as_secs_f32(),
                _ => state_seconds,
            };
            let pulse =
                0.55 + 0.45 * (0.5 + 0.5 * (2.0 * std::f32::consts::PI * elapsed / 1.6).cos());
            let dot_x = pad + OVERLAY_DOT_X_DIP * scale;
            let dot_radius = OVERLAY_DOT_RADIUS_DIP * scale;
            let dot_color = record_color(alpha * pulse);
            if state.overlay_close_visible() {
                let hot = state.overlay_close_hot();
                if let Some(path) = circle_path(dot_x, center_y, 8.0 * scale) {
                    let fill = if hot { 72.0 } else { 40.0 };
                    fill_path(pixmap, &path, rgba8(255, 255, 255, (fill * alpha) as u8));
                }
                let reach = 3.0 * scale;
                for direction in [-1.0, 1.0] {
                    if let Some(path) = line_path(
                        dot_x - reach,
                        center_y - direction * reach,
                        dot_x + reach,
                        center_y + direction * reach,
                    ) {
                        let ink = if hot { 1.0 } else { 0.85 };
                        stroke_path(pixmap, &path, bar_color(alpha * ink), 1.5 * scale);
                    }
                }
            } else if state.settings.push_to_talk {
                if let Some(path) = circle_path(dot_x, center_y, dot_radius) {
                    stroke_path(pixmap, &path, dot_color, 1.5 * scale);
                }
            } else if let Some(path) = circle_path(dot_x, center_y, dot_radius) {
                fill_path(pixmap, &path, dot_color);
            }
            let bar_width = OVERLAY_BAR_WIDTH_DIP * scale;
            for index in 0..OVERLAY_WAVE_BARS {
                let level = state.overlay_shown[index].clamp(0.0, 1.0);
                let bar_height = (OVERLAY_BAR_MIN_DIP
                    + level * (OVERLAY_BAR_MAX_DIP - OVERLAY_BAR_MIN_DIP))
                    * scale;
                let x =
                    pad + (OVERLAY_BAR_START_DIP + index as f32 * OVERLAY_BAR_PITCH_DIP) * scale;
                let y = center_y - bar_height / 2.0;
                if let Some(path) = round_rect_path(x, y, bar_width, bar_height, bar_width / 2.0) {
                    fill_path(pixmap, &path, bar_color(alpha));
                }
            }
            if let Some(remaining) = countdown {
                draw_countdown(pixmap, remaining, pad, pill_width, center_y, scale, alpha);
            }
        }
        Some(OverlayKind::Working) => {
            let dot_x = pad + OVERLAY_DOT_X_DIP * scale;
            if let Some(path) = circle_path(dot_x, center_y, OVERLAY_DOT_RADIUS_DIP * scale) {
                fill_path(pixmap, &path, work_color(alpha));
            }
            let period = state_seconds / 0.9;
            for index in 0..3 {
                let phase = 2.0 * std::f32::consts::PI * (period - index as f32 / 6.0);
                let offset = 3.0 * phase.sin().max(0.0);
                let x = pad + (27.0 + index as f32 * 6.0) * scale;
                let y = center_y - offset * scale;
                if let Some(path) = circle_path(x, y, 1.75 * scale) {
                    fill_path(pixmap, &path, bar_color(alpha));
                }
            }
        }
        _ => {}
    }
}

fn draw_countdown(
    pixmap: &mut Pixmap,
    remaining: u32,
    pad: f32,
    pill_width: f32,
    center_y: f32,
    scale: f32,
    alpha: f32,
) {
    let text = format!("{}:{:02}", remaining / 60, remaining % 60);
    let width = match i32::try_from(pixmap.width()) {
        Ok(width) => width,
        Err(_) => return,
    };
    let height = match i32::try_from(pixmap.height()) {
        Ok(height) => height,
        Err(_) => return,
    };
    let Some(mask_len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return;
    };

    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return;
    }

    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut mask_pixels: *mut core::ffi::c_void = core::ptr::null_mut();
    let bitmap = match unsafe {
        CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut mask_pixels, None, 0)
    } {
        Ok(bitmap) => bitmap,
        Err(_) => {
            unsafe {
                let _ = DeleteDC(dc);
            }
            return;
        }
    };
    if mask_pixels.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        return;
    }
    unsafe { std::slice::from_raw_parts_mut(mask_pixels.cast::<u8>(), mask_len).fill(0) };

    let previous_bitmap = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
    if previous_bitmap.0.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        return;
    }

    let font_height = (11.0 * scale).round().max(1.0) as i32;
    let Some((font, previous_font)) = select_overlay_countdown_font(dc, font_height) else {
        unsafe {
            let _ = SelectObject(dc, previous_bitmap);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        return;
    };

    let text_wide = wide(&text);
    let mut extent = SIZE::default();
    let mut metrics = TEXTMETRICW::default();
    let measured = unsafe {
        GetTextExtentPoint32W(
            dc,
            &text_wide[..text_wide.len().saturating_sub(1)],
            &mut extent,
        )
        .as_bool()
            && GetTextMetricsW(dc, &mut metrics).as_bool()
    };
    if measured {
        let right = pad + pill_width - 10.0 * scale;
        let x = (right - extent.cx as f32).round() as i32;
        let y = (center_y - metrics.tmHeight as f32 / 2.0).round() as i32;
        unsafe {
            let _ = SetBkMode(dc, TRANSPARENT);
            let _ = SetTextColor(dc, COLORREF(0x00FF_FFFF));
        }
        if unsafe { TextOutW(dc, x, y, &text_wide[..text_wide.len().saturating_sub(1)]) }.as_bool()
        {
            composite_countdown_mask(pixmap, mask_pixels, alpha);
        }
    }

    unsafe {
        let _ = SelectObject(dc, previous_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = SelectObject(dc, previous_bitmap);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(dc);
    }
}

/// Selects Segoe UI Variable Semibold when installed, falling back to Segoe UI.
fn select_overlay_countdown_font(dc: HDC, height: i32) -> Option<(HFONT, HGDIOBJ)> {
    for family in ["Segoe UI Variable", "Segoe UI"] {
        let family_wide = wide(family);
        let font = unsafe {
            CreateFontW(
                -height,
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                PCWSTR(family_wide.as_ptr()),
            )
        };
        if font.0.is_null() {
            continue;
        }

        let previous = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
        if previous.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
            continue;
        }

        let mut actual_face = [0_u16; 64];
        let face_length = unsafe { GetTextFaceW(dc, Some(&mut actual_face)) };
        let face_length = actual_face
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(face_length.max(0) as usize)
            .min(actual_face.len());
        let actual_face = String::from_utf16_lossy(&actual_face[..face_length]);
        let matches_requested_family = if family == "Segoe UI Variable" {
            actual_face.starts_with("Segoe UI Variable")
                || actual_face.eq_ignore_ascii_case("Segoe UI")
        } else {
            actual_face.eq_ignore_ascii_case("Segoe UI")
        };
        if matches_requested_family {
            return Some((font, previous));
        }

        unsafe {
            let _ = SelectObject(dc, previous);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }
    None
}

/// Blends the grayscale GDI glyph mask into tiny-skia's premultiplied RGBA data.
fn composite_countdown_mask(
    pixmap: &mut Pixmap,
    mask_pixels: *const core::ffi::c_void,
    alpha: f32,
) {
    let mask = unsafe { std::slice::from_raw_parts(mask_pixels.cast::<u8>(), pixmap.data().len()) };
    let pixmap_data = pixmap.data_mut();
    for (mask_pixel, target_pixel) in mask.chunks_exact(4).zip(pixmap_data.chunks_exact_mut(4)) {
        let coverage =
            (u16::from(mask_pixel[0]) + u16::from(mask_pixel[1]) + u16::from(mask_pixel[2])) as f32
                / (3.0 * 255.0);
        let source_alpha = (coverage * alpha).clamp(0.0, 1.0);
        if source_alpha <= 0.0 {
            continue;
        }
        let inverse_alpha = 1.0 - source_alpha;
        target_pixel[0] = (WORK_RGB.0 as f32 * source_alpha
            + target_pixel[0] as f32 * inverse_alpha)
            .round() as u8;
        target_pixel[1] = (WORK_RGB.1 as f32 * source_alpha
            + target_pixel[1] as f32 * inverse_alpha)
            .round() as u8;
        target_pixel[2] = (WORK_RGB.2 as f32 * source_alpha
            + target_pixel[2] as f32 * inverse_alpha)
            .round() as u8;
        target_pixel[3] =
            (255.0 * source_alpha + target_pixel[3] as f32 * inverse_alpha).round() as u8;
    }
}

/// Fades the error badge out after it has held for two seconds.
fn error_fade(seconds: f32) -> f32 {
    if seconds <= OVERLAY_ERROR_HOLD_SECONDS {
        1.0
    } else {
        (1.0 - (seconds - OVERLAY_ERROR_HOLD_SECONDS) / OVERLAY_ERROR_FADE_SECONDS).clamp(0.0, 1.0)
    }
}

fn rgba8(red: u8, green: u8, blue: u8, alpha: u8) -> Color {
    Color::from_rgba8(red, green, blue, alpha)
}

fn scaled_rgba(rgb: (u8, u8, u8), alpha: f32) -> Color {
    rgba8(rgb.0, rgb.1, rgb.2, (255.0 * alpha).clamp(0.0, 255.0) as u8)
}

fn pill_fill(alpha: f32) -> Color {
    rgba8(
        PILL_FILL_RGB.0,
        PILL_FILL_RGB.1,
        PILL_FILL_RGB.2,
        alpha.clamp(0.0, 255.0) as u8,
    )
}

fn pill_border(alpha: f32) -> Color {
    rgba8(
        PILL_BORDER_RGB.0,
        PILL_BORDER_RGB.1,
        PILL_BORDER_RGB.2,
        (33.0 * alpha).clamp(0.0, 255.0) as u8,
    )
}

fn record_color(alpha: f32) -> Color {
    scaled_rgba(REC_RGB, alpha)
}

fn work_color(alpha: f32) -> Color {
    scaled_rgba(WORK_RGB, alpha)
}

fn bar_color(alpha: f32) -> Color {
    scaled_rgba(BAR_RGB, alpha)
}

fn fill_path(pixmap: &mut Pixmap, path: &Path, color: Color) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn stroke_path(pixmap: &mut Pixmap, path: &Path, color: Color, width: f32) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = width.max(0.1);
    stroke.line_cap = LineCap::Round;
    stroke.line_join = LineJoin::Round;
    pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

/// Builds a rounded rectangle path with cubic corners.
fn round_rect_path(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<Path> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
    let mut builder = PathBuilder::new();
    if radius <= 0.0 {
        builder.move_to(x, y);
        builder.line_to(x + width, y);
        builder.line_to(x + width, y + height);
        builder.line_to(x, y + height);
        builder.close();
        return builder.finish();
    }
    let right = x + width;
    let bottom = y + height;
    let handle = radius * CIRCLE_KAPPA;
    builder.move_to(x + radius, y);
    builder.line_to(right - radius, y);
    builder.cubic_to(
        right - radius + handle,
        y,
        right,
        y + radius - handle,
        right,
        y + radius,
    );
    builder.line_to(right, bottom - radius);
    builder.cubic_to(
        right,
        bottom - radius + handle,
        right - radius + handle,
        bottom,
        right - radius,
        bottom,
    );
    builder.line_to(x + radius, bottom);
    builder.cubic_to(
        x + radius - handle,
        bottom,
        x,
        bottom - radius + handle,
        x,
        bottom - radius,
    );
    builder.line_to(x, y + radius);
    builder.cubic_to(
        x,
        y + radius - handle,
        x + radius - handle,
        y,
        x + radius,
        y,
    );
    builder.close();
    builder.finish()
}

/// Builds a full circle from four cubic arcs.
fn circle_path(center_x: f32, center_y: f32, radius: f32) -> Option<Path> {
    if radius <= 0.0 {
        return None;
    }
    let handle = radius * CIRCLE_KAPPA;
    let mut builder = PathBuilder::new();
    builder.move_to(center_x, center_y - radius);
    builder.cubic_to(
        center_x + handle,
        center_y - radius,
        center_x + radius,
        center_y - handle,
        center_x + radius,
        center_y,
    );
    builder.cubic_to(
        center_x + radius,
        center_y + handle,
        center_x + handle,
        center_y + radius,
        center_x,
        center_y + radius,
    );
    builder.cubic_to(
        center_x - handle,
        center_y + radius,
        center_x - radius,
        center_y + handle,
        center_x - radius,
        center_y,
    );
    builder.cubic_to(
        center_x - radius,
        center_y - handle,
        center_x - handle,
        center_y - radius,
        center_x,
        center_y - radius,
    );
    builder.close();
    builder.finish()
}

fn line_path(x1: f32, y1: f32, x2: f32, y2: f32) -> Option<Path> {
    let mut builder = PathBuilder::new();
    builder.move_to(x1, y1);
    builder.line_to(x2, y2);
    builder.finish()
}

/// Presents a premultiplied RGBA pixmap through UpdateLayeredWindow.
///
/// A temporary top-down 32-bit DIB receives a channel-swapped copy because a
/// layered window expects premultiplied BGRA. The DIB and its device context are
/// released before returning, so no GDI objects survive between frames.
pub(super) fn present_layered_window(hwnd: HWND, pixmap: &Pixmap) -> Result<(), String> {
    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    if width <= 0 || height <= 0 {
        return Ok(());
    }

    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.0.is_null() {
        return Err("could not acquire a screen device context".to_owned());
    }
    let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if memory_dc.0.is_null() {
        unsafe {
            let _ = ReleaseDC(None, screen_dc);
        }
        return Err("could not create an overlay device context".to_owned());
    }

    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // negative height requests a top-down DIB
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut pixels: *mut core::ffi::c_void = core::ptr::null_mut();
    let bitmap = match unsafe {
        CreateDIBSection(Some(screen_dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0)
    } {
        Ok(bitmap) => bitmap,
        Err(error) => {
            unsafe {
                let _ = DeleteDC(memory_dc);
                let _ = ReleaseDC(None, screen_dc);
            }
            return Err(format!("could not create the overlay bitmap: {error}"));
        }
    };

    let source = pixmap.data();
    let pixel_count = width as usize * height as usize;
    if !pixels.is_null() && source.len() >= pixel_count * 4 {
        let destination =
            unsafe { std::slice::from_raw_parts_mut(pixels as *mut u8, pixel_count * 4) };
        for index in 0..pixel_count {
            let offset = index * 4;
            destination[offset] = source[offset + 2];
            destination[offset + 1] = source[offset + 1];
            destination[offset + 2] = source[offset];
            destination[offset + 3] = source[offset + 3];
        }
    }

    let previous = unsafe { SelectObject(memory_dc, HGDIOBJ(bitmap.0)) };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let size = SIZE {
        cx: width,
        cy: height,
    };
    let origin = POINT { x: 0, y: 0 };
    let result = unsafe {
        UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            None,
            Some(&size as *const SIZE),
            Some(memory_dc),
            Some(&origin as *const POINT),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };

    unsafe {
        let _ = SelectObject(memory_dc, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
    result.map_err(|error| format!("UpdateLayeredWindow failed: {error}"))
}

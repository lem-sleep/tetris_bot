//! Native Win32 layered-window overlay for drawing ghost pieces.
//!
//! Uses `UpdateLayeredWindow` with per-pixel alpha — the standard
//! technique for transparent click-through game overlays on Windows.

use std::sync::{atomic::Ordering, Arc};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::ai::OverlayPlacement;
use crate::BotState;

// Padding around the board so the HOLD indicator fits.
const PAD_LEFT: i32 = 120;
const PAD_TOP: i32 = 10;
const PAD_RIGHT: i32 = 10;
const PAD_BOTTOM: i32 = 10;

use crate::vision::CellColor;

/// Returns a **pre-multiplied** ARGB u32 for a piece at the given opacity.
fn ghost_pixel(piece: CellColor, opacity: u8) -> u32 {
    let (r, g, b) = match piece {
        CellColor::I => (0u8, 220, 220),
        CellColor::O => (220, 220, 0),
        CellColor::T => (160, 0, 220),
        CellColor::S => (0, 220, 0),
        CellColor::Z => (220, 0, 0),
        CellColor::J => (0, 0, 220),
        CellColor::L => (220, 130, 0),
        _ => (180, 180, 180),
    };
    premultiply(r, g, b, opacity)
}

/// Brighter outline variant.
fn outline_pixel(piece: CellColor, opacity: u8) -> u32 {
    ghost_pixel(piece, opacity.saturating_add(80))
}

/// Draw a single placement's ghost cells onto the pixel buffer.
fn draw_ghost_cells(
    pixels: &mut [u32],
    w: usize,
    h: usize,
    pl: &OverlayPlacement,
    board_x: f32,
    board_y: f32,
    cell_sz: f32,
    opacity: u8,
) {
    let fill = ghost_pixel(pl.piece_color, opacity);
    let outline = outline_pixel(pl.piece_color, opacity);

    for &(col, row) in &pl.cells {
        if col < 0 || col >= 10 || row < 0 || row >= 20 {
            continue;
        }
        let px = (board_x + col as f32 * cell_sz) as usize;
        let py = (board_y + row as f32 * cell_sz) as usize;
        let cw = cell_sz as usize;
        let ch = cell_sz as usize;

        for y in py..(py + ch).min(h) {
            for x in px..(px + cw).min(w) {
                let border = x < px + 2 || x >= px + cw - 2 || y < py + 2 || y >= py + ch - 2;
                pixels[y * w + x] = if border { outline } else { fill };
            }
        }
    }
}

/// Pre-multiply RGB by alpha and pack as 0xAARRGGBB (BGRA in memory on LE).
fn premultiply(r: u8, g: u8, b: u8, a: u8) -> u32 {
    let aa = a as u32;
    let rr = (r as u32 * aa / 255) as u8;
    let gg = (g as u32 * aa / 255) as u8;
    let bb = (b as u32 * aa / 255) as u8;
    (aa << 24) | ((rr as u32) << 16) | ((gg as u32) << 8) | bb as u32
}

/// Spawn the overlay on the calling thread (blocks forever).
pub fn run_overlay(
    state: Arc<BotState>,
    board_screen_x: f32,
    board_screen_y: f32,
    cell_size: f32,
    ghost_opacity: u8,
) {
    unsafe {
        overlay_loop(state, board_screen_x, board_screen_y, cell_size, ghost_opacity);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn overlay_loop(
    state: Arc<BotState>,
    board_x: f32,
    board_y: f32,
    cell_sz: f32,
    opacity: u8,
) {
    // ---- window class ----
    let class_name = w!("TetrisESPOverlay");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassW(&wc);

    // ---- window dimensions (covers board + margins) ----
    let board_pw = (10.0 * cell_sz).ceil() as i32;
    let board_ph = (20.0 * cell_sz).ceil() as i32;

    let win_x = (board_x as i32 - PAD_LEFT).max(0);
    let win_y = (board_y as i32 - PAD_TOP).max(0);
    let win_w = board_pw + PAD_LEFT + PAD_RIGHT;
    let win_h = board_ph + PAD_TOP + PAD_BOTTOM;

    // Local-coordinate origin of the board inside the bitmap.
    let local_board_x = PAD_LEFT as f32;
    let local_board_y = PAD_TOP as f32;

    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class_name,
        w!(""),
        WS_POPUP,
        win_x,
        win_y,
        win_w,
        win_h,
        None,
        None,
        None,
        None,
    )
    .expect("CreateWindowExW failed");

    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    // ---- 32-bit top-down DIB section for per-pixel alpha ----
    let w = win_w as usize;
    let h = win_h as usize;

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    let screen_dc = GetDC(None);
    let mem_dc = CreateCompatibleDC(screen_dc);
    let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
        .expect("CreateDIBSection failed");
    let _old = SelectObject(mem_dc, dib);

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let pt_dst = POINT { x: win_x, y: win_y };
    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE {
        cx: w as i32,
        cy: h as i32,
    };

    let pixel_count = w * h;

    // ---- render loop ----
    loop {
        // Pump messages so Windows doesn't consider the window hung.
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Clear bitmap to fully transparent.
        let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, pixel_count);
        pixels.fill(0);

        let enabled = state.enabled.load(Ordering::Relaxed);
        let paused = state.paused.load(Ordering::Relaxed);

        if enabled && !paused {
            let suggestion = state.current_suggestion.lock().unwrap().clone();

            if let Some(sg) = suggestion {
                // Draw direct placement (current piece, full opacity).
                draw_ghost_cells(
                    pixels, w, h,
                    &sg.direct, local_board_x, local_board_y, cell_sz, opacity,
                );

                // Draw hold placement (dimmer, to distinguish from direct).
                if let Some(ref hold_pl) = sg.hold_option {
                    let hold_opacity = opacity.saturating_sub(40).max(40);
                    draw_ghost_cells(
                        pixels, w, h,
                        hold_pl, local_board_x, local_board_y, cell_sz, hold_opacity,
                    );
                }
            }
        }

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            Some(&pt_dst),
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

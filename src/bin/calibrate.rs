//! Calibration tool for the TETR.IO bot.
//!
//! Captures a screenshot, saves it to disk, and optionally samples pixel colors
//! at specified coordinates. Use this to determine the correct board_x, board_y,
//! cell_size, hold position, and next queue positions for your config.toml.
//!
//! Usage:
//!   cargo run --bin calibrate                    # Capture screenshot
//!   cargo run --bin calibrate -- sample 735 155  # Sample color at (735, 155)
//!   cargo run --bin calibrate -- grid 655 155 33 # Show board grid overlay info

use anyhow::{Result, Context};
use dxgi_capture_rs::DXGIManager;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    println!("=== TETR.IO Bot Calibration Tool ===\n");

    let mut manager = DXGIManager::new(1000)
        .context("Failed to initialize DXGI")?;

    let (screen_w, screen_h) = manager.geometry();
    println!("Screen resolution: {screen_w}x{screen_h}");

    // Wait a moment for the desktop to be ready
    std::thread::sleep(std::time::Duration::from_millis(500));

    println!("Capturing screen...");
    let (pixels, (w, h)) = manager.capture_frame_components()
        .map_err(|e| anyhow::anyhow!("Capture failed: {:?}", e))?;

    println!("Captured frame: {w}x{h} ({} bytes)\n", pixels.len());

    match args.get(1).map(|s| s.as_str()) {
        Some("sample") => {
            let x: u32 = args.get(2)
                .context("Usage: calibrate sample <x> <y>")?
                .parse().context("Invalid x coordinate")?;
            let y: u32 = args.get(3)
                .context("Usage: calibrate sample <x> <y>")?
                .parse().context("Invalid y coordinate")?;

            sample_pixel(&pixels, w as u32, x, y);
        }
        Some("grid") => {
            let bx: u32 = args.get(2)
                .context("Usage: calibrate grid <board_x> <board_y> <cell_size>")?
                .parse()?;
            let by: u32 = args.get(3)
                .context("Usage: calibrate grid <board_x> <board_y> <cell_size>")?
                .parse()?;
            let cs: u32 = args.get(4)
                .context("Usage: calibrate grid <board_x> <board_y> <cell_size>")?
                .parse()?;

            print_grid_samples(&pixels, w as u32, bx, by, cs);
        }
        Some("scan") => {
            // Scan a row of pixels to find board edges
            let y: u32 = args.get(2)
                .context("Usage: calibrate scan <y> [x_start] [x_end]")?
                .parse()?;
            let x_start: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            let x_end: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(w as u32);

            scan_row(&pixels, w as u32, y, x_start, x_end);
        }
        _ => {
            // Default: save screenshot and print instructions
            save_screenshot(&pixels, w as u32, h as u32)?;
        }
    }

    Ok(())
}

fn save_screenshot(pixels: &[u8], w: u32, h: u32) -> Result<()> {
    // Convert BGRA to RGBA for the image crate
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        rgba[i * 4] = pixels[i * 4 + 2];     // R
        rgba[i * 4 + 1] = pixels[i * 4 + 1]; // G
        rgba[i * 4 + 2] = pixels[i * 4];     // B
        rgba[i * 4 + 3] = pixels[i * 4 + 3]; // A
    }

    let path = "calibration_screenshot.png";
    image::save_buffer(path, &rgba, w, h, image::ColorType::Rgba8)
        .context("Failed to save screenshot")?;

    println!("Screenshot saved to: {path}");
    println!();
    println!("Next steps:");
    println!("  1. Open {path} in an image editor (Paint, GIMP, etc.)");
    println!("  2. Note the pixel coordinates of:");
    println!("     - Top-left corner of the board grid (first cell's top-left)");
    println!("     - Size of one cell in pixels");
    println!("     - Center of the hold piece display");
    println!("     - Center of each next queue piece");
    println!("  3. Use 'calibrate sample <x> <y>' to verify colors");
    println!("  4. Use 'calibrate grid <board_x> <board_y> <cell_size>' to verify grid alignment");
    println!("  5. Update config.toml with the correct values");

    Ok(())
}

fn sample_pixel(pixels: &[u8], stride_w: u32, x: u32, y: u32) {
    let offset = (y * stride_w * 4 + x * 4) as usize;
    if offset + 3 >= pixels.len() {
        println!("ERROR: coordinates ({x}, {y}) are out of bounds");
        return;
    }
    let b = pixels[offset];
    let g = pixels[offset + 1];
    let r = pixels[offset + 2];

    let (h, s, v) = rgb_to_hsv(r, g, b);
    let piece = classify_piece(r, g, b);
    let ghost_note = if v < 0.35 && v >= 0.15 { " ← ghost piece range (filtered)" } else { "" };

    println!("Pixel at ({x}, {y}):");
    println!("  RGB: ({r}, {g}, {b})");
    println!("  HSV: hue={h:.0}  sat={s:.2}  val={v:.2}{ghost_note}");
    println!("  Classified as: {piece}");

    // Also sample a 3x3 area around the point
    println!("\n3x3 neighborhood:");
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let sx = (x as i32 + dx) as u32;
            let sy = (y as i32 + dy) as u32;
            let off = (sy * stride_w * 4 + sx * 4) as usize;
            if off + 3 < pixels.len() {
                let br = pixels[off + 2];
                let bg = pixels[off + 1];
                let bb = pixels[off];
                print!("  ({br:3},{bg:3},{bb:3})");
            }
        }
        println!();
    }
}

/// Sample a cell at 5 points (center + NESW) and return the majority classification.
/// Marks uncertain cells (< 3/5 agreement) with a '?' suffix.
fn sample_cell_votes(pixels: &[u8], stride_w: u32, cx: u32, cy: u32, off: u32) -> (&'static str, u8) {
    let offsets: [(i32, i32); 5] = [(0, 0), (-(off as i32), 0), (off as i32, 0), (0, -(off as i32)), (0, off as i32)];
    let mut counts = [0u8; 10]; // indexed by piece symbol index
    let labels = [".", "I", "O", "T", "S", "Z", "J", "L", "G", "?"];

    for &(dx, dy) in &offsets {
        let x = (cx as i32 + dx).max(0) as u32;
        let y = (cy as i32 + dy).max(0) as u32;
        let offset = (y * stride_w * 4 + x * 4) as usize;
        if offset + 3 < pixels.len() {
            let r = pixels[offset + 2];
            let g = pixels[offset + 1];
            let b = pixels[offset];
            let label = classify_piece(r, g, b);
            let idx = labels.iter().position(|&l| l == label).unwrap_or(9);
            counts[idx] = counts[idx].saturating_add(1);
        }
    }

    // Find best non-empty vote
    let mut best_idx = 0usize; // empty
    let mut best_count = 0u8;
    for i in 1..9 { // skip empty (0) and unknown (9)
        if counts[i] > best_count {
            best_count = counts[i];
            best_idx = i;
        }
    }
    if best_count == 0 { best_idx = 0; best_count = counts[0]; }
    (labels[best_idx], best_count)
}

fn print_grid_samples(pixels: &[u8], stride_w: u32, bx: u32, by: u32, cs: u32) {
    println!("Board grid (5-point vote, center ± {off}px):\n", off = cs / 4);
    println!("Config: board_x={bx}, board_y={by}, cell_size={cs}");
    println!("Legend: letter = piece,  . = empty,  * = uncertain (< 3/5 votes)\n");

    print!("     ");
    for col in 0..10 { print!(" {col:^3} "); }
    println!();

    let off = cs / 4;
    for row in 0..20u32 {
        print!("R{row:02} ");
        for col in 0..10u32 {
            let cx = bx + col * cs + cs / 2;
            let cy = by + row * cs + cs / 2;
            let (piece, votes) = sample_cell_votes(pixels, stride_w, cx, cy, off);
            let uncertain = votes < 3 && piece != ".";
            print!(" {:^3} ", if uncertain { "*" } else { piece });
        }
        println!();
    }
    println!("\n(Run with 'sample <x> <y>' to inspect individual pixels)");
}

fn scan_row(pixels: &[u8], stride_w: u32, y: u32, x_start: u32, x_end: u32) {
    println!("Scanning row y={y} from x={x_start} to x={x_end}");
    println!("Looking for transitions (bright/dark edges)...\n");

    let mut prev_brightness: Option<u16> = None;
    let threshold = 30u16;

    for x in x_start..x_end {
        let offset = (y * stride_w * 4 + x * 4) as usize;
        if offset + 3 >= pixels.len() { break; }
        let r = pixels[offset + 2] as u16;
        let g = pixels[offset + 1] as u16;
        let b = pixels[offset] as u16;
        let brightness = (r + g + b) / 3;

        if let Some(prev) = prev_brightness {
            let diff = (brightness as i32 - prev as i32).unsigned_abs() as u16;
            if diff > threshold {
                println!("  Edge at x={x}: brightness {prev} -> {brightness} (RGB: {},{},{})",
                    pixels[offset + 2], pixels[offset + 1], pixels[offset]);
            }
        }
        prev_brightness = Some(brightness);
    }
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let v = max;
    let s = if max == 0.0 { 0.0 } else { delta / max };
    let h = if delta == 0.0 { 0.0 }
        else if max == r { 60.0 * (((g - b) / delta) % 6.0) }
        else if max == g { 60.0 * (((b - r) / delta) + 2.0) }
        else { 60.0 * (((r - g) / delta) + 4.0) };
    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s, v)
}

/// Classify a pixel — kept in sync with vision/mod.rs classify_rgb().
/// Same ghost-piece filter (v < 0.35), same hue ranges, same saturation threshold.
fn classify_piece(r: u8, g: u8, b: u8) -> &'static str {
    let brightness = (r as u16 + g as u16 + b as u16) / 3;
    if brightness < 30 { return "."; }

    let (h, s, v) = rgb_to_hsv(r, g, b);

    // Ghost-piece filter: same threshold as the bot uses
    if v < 0.35 { return "."; }

    if s < 0.22 {
        return if brightness > 80 { "G" } else { "." };
    }

    match h as u32 {
        0..=15 | 345..=360 => "Z",
        16..=45            => "L",
        46..=75            => "O",
        76..=150           => "S",
        151..=195          => "I",
        196..=265          => "J",
        266..=344          => "T",
        _                  => ".",
    }
}

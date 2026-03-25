//! Analyze an existing screenshot PNG to find board coordinates.
//! Usage: cargo run --bin analyze [path_to_png]

use anyhow::{Result, Context};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(|s| s.as_str()).unwrap_or("calibration_screenshot.png");

    println!("=== Analyzing: {path} ===\n");

    let img = image::open(path).context("Failed to open image")?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    println!("Image size: {w}x{h}\n");

    let cmd = args.get(2).map(|s| s.as_str()).unwrap_or("find");

    match cmd {
        "sample" => {
            let x: u32 = args[3].parse()?;
            let y: u32 = args[4].parse()?;
            sample(&rgba, x, y);
        }
        "grid" => {
            let bx: u32 = args[3].parse()?;
            let by: u32 = args[4].parse()?;
            let cs: u32 = args[5].parse()?;
            print_grid(&rgba, bx, by, cs);
        }
        "hscan" => {
            let y: u32 = args[3].parse()?;
            let x0: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
            let x1: u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(w);
            hscan(&rgba, y, x0, x1);
        }
        "vscan" => {
            let x: u32 = args[3].parse()?;
            let y0: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
            let y1: u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(h);
            vscan(&rgba, x, y0, y1);
        }
        _ => {
            // Auto-find board edges
            find_board(&rgba);
        }
    }

    Ok(())
}

fn find_board(img: &image::RgbaImage) {
    let (w, h) = (img.width(), img.height());
    let mid_y = h / 2;
    let mid_x = w / 2;

    println!("--- Auto-detecting board edges ---\n");

    // Scan horizontally at mid_y to find left/right edges of the board
    println!("Horizontal scan at y={mid_y}:");
    let mut left_edge = None;
    let mut right_edge = None;
    let mut prev_bright: Option<u16> = None;

    for x in 0..w {
        let p = img.get_pixel(x, mid_y);
        let bright = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
        if let Some(pb) = prev_bright {
            let diff = (bright as i32 - pb as i32).unsigned_abs() as u16;
            if diff > 20 {
                if left_edge.is_none() && bright > pb {
                    left_edge = Some(x);
                } else if left_edge.is_some() && bright > pb && right_edge.is_none() {
                    // Keep tracking — we want the last significant drop->rise transition
                }
                if left_edge.is_some() && bright < pb {
                    right_edge = Some(x);
                }
            }
        }
        prev_bright = Some(bright);
    }
    println!("  Left edge candidate: {:?}", left_edge);
    println!("  Right edge candidate: {:?}", right_edge);

    // Scan vertically at mid_x to find top/bottom edges
    println!("\nVertical scan at x={mid_x}:");
    let mut top_edge = None;
    let mut bottom_edge = None;
    prev_bright = None;

    for y in 0..h {
        let p = img.get_pixel(mid_x, y);
        let bright = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
        if let Some(pb) = prev_bright {
            let diff = (bright as i32 - pb as i32).unsigned_abs() as u16;
            if diff > 20 {
                if top_edge.is_none() {
                    top_edge = Some(y);
                }
                bottom_edge = Some(y);
            }
        }
        prev_bright = Some(bright);
    }
    println!("  Top edge candidate: {:?}", top_edge);
    println!("  Bottom edge candidate: {:?}", bottom_edge);

    // Now do detailed scans around the edges
    if let (Some(left), Some(top), Some(right), Some(bottom)) = (left_edge, top_edge, right_edge, bottom_edge) {
        let board_w = right - left;
        let board_h = bottom - top;
        let cell_w = board_w / 10;
        let cell_h = board_h / 20;
        println!("\n--- Estimated board geometry ---");
        println!("  Top-left: ({left}, {top})");
        println!("  Bottom-right: ({right}, {bottom})");
        println!("  Board pixels: {board_w} x {board_h}");
        println!("  Cell size (w): {cell_w}");
        println!("  Cell size (h): {cell_h}");
        println!();

        // Sample a few cells to verify
        println!("--- Sample cells (center pixel) ---");
        for row in [0u32, 5, 10, 15, 19] {
            for col in [0u32, 4, 9] {
                let cx = left + col * cell_w + cell_w / 2;
                let cy = top + row * cell_h + cell_h / 2;
                let p = img.get_pixel(cx, cy);
                let piece = classify(p[0], p[1], p[2]);
                print!("  [{row},{col}]=({cx},{cy}):{piece}");
            }
            println!();
        }
    }

    // Look for HOLD box — scan left of the board
    println!("\n--- Scanning for HOLD/NEXT boxes ---");
    if let Some(left) = left_edge {
        // Sample points left of the board for the hold piece
        println!("  Hold area candidates (sampling left of board):");
        for y in (50..400).step_by(20) {
            for x in (left.saturating_sub(250)..left.saturating_sub(20)).step_by(20) {
                let p = img.get_pixel(x, y);
                let bright = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
                if bright > 60 {
                    let piece = classify(p[0], p[1], p[2]);
                    if piece != "Empty" && piece != "Garbage" {
                        println!("    Colored pixel at ({x}, {y}): RGB({},{},{}) = {piece}",
                            p[0], p[1], p[2]);
                    }
                }
            }
        }
    }

    if let Some(right) = right_edge {
        println!("  Next queue candidates (sampling right of board):");
        for y in (50..800).step_by(10) {
            for x in (right + 20..right.saturating_add(250)).step_by(20) {
                if x >= w { continue; }
                let p = img.get_pixel(x, y);
                let bright = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
                if bright > 60 {
                    let piece = classify(p[0], p[1], p[2]);
                    if piece != "Empty" && piece != "Garbage" {
                        println!("    Colored pixel at ({x}, {y}): RGB({},{},{}) = {piece}",
                            p[0], p[1], p[2]);
                    }
                }
            }
        }
    }
}

fn sample(img: &image::RgbaImage, x: u32, y: u32) {
    let p = img.get_pixel(x, y);
    let (h, s, v) = rgb_to_hsv(p[0], p[1], p[2]);
    println!("Pixel ({x}, {y}): RGB({}, {}, {})  HSV({h:.0}, {s:.2}, {v:.2})  => {}",
        p[0], p[1], p[2], classify(p[0], p[1], p[2]));

    println!("5x5 neighborhood:");
    for dy in -2i32..=2 {
        for dx in -2i32..=2 {
            let sx = (x as i32 + dx).clamp(0, img.width() as i32 - 1) as u32;
            let sy = (y as i32 + dy).clamp(0, img.height() as i32 - 1) as u32;
            let p = img.get_pixel(sx, sy);
            print!(" ({:3},{:3},{:3})", p[0], p[1], p[2]);
        }
        println!();
    }
}

fn print_grid(img: &image::RgbaImage, bx: u32, by: u32, cs: u32) {
    println!("Board grid: top-left=({bx},{by}) cell_size={cs}\n");
    print!("     ");
    for c in 0..10 { print!(" {:^3} ", c); }
    println!();

    for row in 0..20u32 {
        print!("R{row:02} ");
        for col in 0..10u32 {
            let cx = bx + col * cs + cs / 2;
            let cy = by + row * cs + cs / 2;
            let p = img.get_pixel(cx, cy);
            let piece = classify(p[0], p[1], p[2]);
            let ch = match piece {
                "Empty" => ".",
                "I" => "I", "O" => "O", "T" => "T",
                "S" => "S", "Z" => "Z", "J" => "J", "L" => "L",
                "Garbage" => "G",
                _ => "?",
            };
            print!("  {ch}  ");
        }
        println!();
    }
}

fn hscan(img: &image::RgbaImage, y: u32, x0: u32, x1: u32) {
    println!("Horizontal scan y={y}, x=[{x0}..{x1}]");
    let mut prev: Option<u16> = None;
    for x in x0..x1 {
        let p = img.get_pixel(x, y);
        let b = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
        if let Some(pb) = prev {
            let d = (b as i32 - pb as i32).unsigned_abs() as u16;
            if d > 15 {
                println!("  x={x}: {pb}->{b} (d={d}) RGB({},{},{})", p[0], p[1], p[2]);
            }
        }
        prev = Some(b);
    }
}

fn vscan(img: &image::RgbaImage, x: u32, y0: u32, y1: u32) {
    println!("Vertical scan x={x}, y=[{y0}..{y1}]");
    let mut prev: Option<u16> = None;
    for y in y0..y1 {
        let p = img.get_pixel(x, y);
        let b = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
        if let Some(pb) = prev {
            let d = (b as i32 - pb as i32).unsigned_abs() as u16;
            if d > 15 {
                println!("  y={y}: {pb}->{b} (d={d}) RGB({},{},{})", p[0], p[1], p[2]);
            }
        }
        prev = Some(b);
    }
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let v = max;
    let s = if max == 0.0 { 0.0 } else { d / max };
    let h = if d == 0.0 { 0.0 }
        else if max == r { 60.0 * (((g - b) / d) % 6.0) }
        else if max == g { 60.0 * (((b - r) / d) + 2.0) }
        else { 60.0 * (((r - g) / d) + 4.0) };
    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s, v)
}

fn classify(r: u8, g: u8, b: u8) -> &'static str {
    let bright = (r as u16 + g as u16 + b as u16) / 3;
    if bright < 30 { return "Empty"; }
    let (h, s, _) = rgb_to_hsv(r, g, b);
    if s < 0.15 { return if bright > 80 { "Garbage" } else { "Empty" }; }
    match h as u32 {
        0..=15 | 345..=360 => "Z",
        16..=45 => "L",
        46..=70 => "O",
        71..=160 => "S",
        161..=200 => "I",
        201..=260 => "J",
        261..=330 => "T",
        _ => "?",
    }
}

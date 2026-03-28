//! Board reading / computer vision module.
//!
//! Extracts the full game state from a captured frame by sampling pixel colors
//! at known grid positions. Uses a precomputed RGB→CellColor LUT for O(1)
//! classification, multi-point sampling per cell for noise tolerance, and a
//! ghost-piece filter so TETR.IO's dim ghost overlay doesn't pollute the board.

use anyhow::Result;
use crate::capture::Frame;
use crate::config::VisionConfig;

/// The 7 standard Tetris pieces + empty + garbage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CellColor {
    Empty   = 0,
    I       = 1,
    O       = 2,
    T       = 3,
    S       = 4,
    Z       = 5,
    J       = 6,
    L       = 7,
    Garbage = 8,
}

/// Full game state extracted from a single frame.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GameState {
    pub board:            [[CellColor; 10]; 20],
    pub current_piece:    Option<CellColor>,
    pub hold_piece:       Option<CellColor>,
    pub next_queue:       Vec<CellColor>,
    pub is_active:        bool,
    pub incoming_garbage: u32,
}

// ─────────────────────────────────────────────
//  Color LUT
// ─────────────────────────────────────────────

/// Precomputed RGB → CellColor lookup table.
/// Indexed by (r >> 2, g >> 2, b >> 2) → 64×64×64 = 262 144 entries.
struct ColorLut {
    table: Vec<CellColor>,
}

impl ColorLut {
    fn new() -> Self {
        let mut table = vec![CellColor::Empty; 64 * 64 * 64];
        for ri in 0..64u32 {
            for gi in 0..64u32 {
                for bi in 0..64u32 {
                    let r = (ri * 4 + 2) as u8;
                    let g = (gi * 4 + 2) as u8;
                    let b = (bi * 4 + 2) as u8;
                    let idx = (ri * 64 * 64 + gi * 64 + bi) as usize;
                    table[idx] = classify_color_slow(r, g, b);
                }
            }
        }
        Self { table }
    }

    #[inline(always)]
    fn classify(&self, r: u8, g: u8, b: u8) -> CellColor {
        let idx = ((r as usize) >> 2) * 4096
                + ((g as usize) >> 2) * 64
                + ((b as usize) >> 2);
        unsafe { *self.table.get_unchecked(idx) }
    }
}

// ─────────────────────────────────────────────
//  BoardReader
// ─────────────────────────────────────────────

#[allow(dead_code)]
pub struct BoardReader {
    board_x:    u32,
    board_y:    u32,
    cell_size:  u32,
    hold_x:     u32,
    hold_y:     u32,
    next_positions: Vec<(u32, u32)>,
    lut:        ColorLut,
    /// Precomputed center pixel coordinates for each of the 200 board cells.
    cell_coords: [(u32, u32); 200],
    /// Sentinel pixels for fast dirty-frame detection.
    sentinel_coords:      Vec<(u32, u32)>,
    last_sentinel_colors: Vec<(u8, u8, u8)>,
}

impl BoardReader {
    pub fn new(cfg: &VisionConfig) -> Result<Self> {
        let lut = ColorLut::new();

        let mut cell_coords = [(0u32, 0u32); 200];
        for row in 0..20u32 {
            for col in 0..10u32 {
                let px = cfg.board_x + col * cfg.cell_size + cfg.cell_size / 2;
                let py = cfg.board_y + row * cfg.cell_size + cfg.cell_size / 2;
                cell_coords[(row * 10 + col) as usize] = (px, py);
            }
        }

        // Sentinel pixels: scatter across the board + hold + first next slot
        let mut sentinels = vec![
            cell_coords[0  * 10 + 4],   // row 0,  col 4  (top spawn zone)
            cell_coords[0  * 10 + 5],   // row 0,  col 5
            cell_coords[4  * 10 + 4],   // row 4,  col 4
            cell_coords[10 * 10 + 5],   // row 10, col 5  (mid board)
            cell_coords[15 * 10 + 3],   // row 15, col 3
            cell_coords[19 * 10 + 0],   // bottom-left
            cell_coords[19 * 10 + 9],   // bottom-right
            (cfg.hold_x, cfg.hold_y),
        ];
        if let Some(&(nx, ny)) = cfg.next_positions.first() {
            sentinels.push((nx, ny));
        }

        Ok(Self {
            board_x:  cfg.board_x,
            board_y:  cfg.board_y,
            cell_size: cfg.cell_size,
            hold_x:   cfg.hold_x,
            hold_y:   cfg.hold_y,
            next_positions: cfg.next_positions.clone(),
            lut,
            cell_coords,
            sentinel_coords:      sentinels,
            last_sentinel_colors: Vec::new(),
        })
    }

    // ── Dirty-frame fast path ─────────────────

    pub fn is_frame_dirty(&mut self, frame: &Frame) -> bool {
        let current: Vec<(u8, u8, u8)> = self.sentinel_coords.iter()
            .map(|&(x, y)| frame_pixel_safe(frame, x, y))
            .collect();
        if current == self.last_sentinel_colors {
            return false;
        }
        self.last_sentinel_colors = current;
        true
    }

    // ── Public entry point ────────────────────

    pub fn read_state(&self, frame: &Frame) -> Result<GameState> {
        let board         = self.read_board(frame);
        let current_piece = self.detect_current_piece(&board);
        let hold_piece    = self.read_hold_piece(frame);
        let next_queue    = self.read_next_queue(frame);
        let is_active     = self.detect_game_active(&board);

        Ok(GameState {
            board,
            current_piece,
            hold_piece,
            next_queue,
            is_active,
            incoming_garbage: 0,
        })
    }

    // ── Board reading ─────────────────────────

    /// Sample all 200 cells using multi-point voting.
    fn read_board(&self, frame: &Frame) -> [[CellColor; 10]; 20] {
        let mut board = [[CellColor::Empty; 10]; 20];
        for row in 0..20 {
            for col in 0..10 {
                let (cx, cy) = self.cell_coords[row * 10 + col];
                board[row][col] = self.sample_cell(frame, cx, cy, self.cell_size);
            }
        }
        board
    }

    /// Sample a cell at 5 points (center + NESW at ±cell_size/4) and majority-vote.
    ///
    /// Requires at least 2 of 5 samples to agree on the same non-Empty color
    /// before declaring it non-empty — filters single-pixel noise and border hits.
    fn sample_cell(&self, frame: &Frame, cx: u32, cy: u32, cell_sz: u32) -> CellColor {
        let off = (cell_sz / 4).max(4) as i32;
        let pts: [(i32, i32); 5] = [(0, 0), (-off, 0), (off, 0), (0, -off), (0, off)];

        let mut counts = [0u8; 9]; // index = CellColor as u8
        for &(dx, dy) in &pts {
            let x = (cx as i32 + dx).clamp(0, frame.width  as i32 - 1) as u32;
            let y = (cy as i32 + dy).clamp(0, frame.height as i32 - 1) as u32;
            let (r, g, b) = frame.pixel_rgb(x, y);
            let c = self.lut.classify(r, g, b);
            counts[c as usize] = counts[c as usize].saturating_add(1);
        }

        // Find the non-Empty color with the highest vote count
        let mut best_color = CellColor::Empty;
        let mut best_count = 0u8;
        for i in 1u8..9 { // skip 0 = Empty
            if counts[i as usize] > best_count {
                best_count = counts[i as usize];
                best_color = u8_to_cell_color(i);
            }
        }

        // Need at least 2/5 samples to agree — avoids single bad pixels
        if best_count >= 2 { best_color } else { CellColor::Empty }
    }

    // ── Current piece detection ───────────────

    /// Find the falling piece color by scanning rows from the top.
    ///
    /// The active piece always spawns at the top of the board (rows 0-3).
    /// Scanning top-down means the first piece-color cell found is almost
    /// certainly the falling piece, not locked stack cells.
    fn detect_current_piece(&self, board: &[[CellColor; 10]; 20]) -> Option<CellColor> {
        // Priority: scan the spawn zone (top 4 rows) first.
        for row in 0..4 {
            for col in 0..10 {
                let c = board[row][col];
                if c != CellColor::Empty && c != CellColor::Garbage {
                    return Some(c);
                }
            }
        }
        // Piece may have fallen lower — scan the rest of the board.
        for row in 4..20 {
            for col in 0..10 {
                let c = board[row][col];
                if c != CellColor::Empty && c != CellColor::Garbage {
                    return Some(c);
                }
            }
        }
        None
    }

    // ── Hold piece reading ────────────────────

    /// Read the hold piece using a 3×3 grid of sample points around (hold_x, hold_y).
    ///
    /// TETR.IO renders the hold piece in a small preview box. Sampling a grid
    /// rather than a single center pixel tolerates slight coordinate offsets and
    /// edge cases where the center lands on a piece border or background.
    fn read_hold_piece(&self, frame: &Frame) -> Option<CellColor> {
        let color = self.sample_region_vote(frame, self.hold_x, self.hold_y, 8);
        if color != CellColor::Empty { Some(color) } else { None }
    }

    // ── Next queue reading ────────────────────

    fn read_next_queue(&self, frame: &Frame) -> Vec<CellColor> {
        self.next_positions.iter()
            .filter_map(|&(x, y)| {
                let c = self.sample_region_vote(frame, x, y, 8);
                if c != CellColor::Empty { Some(c) } else { None }
            })
            .collect()
    }

    // ── Shared sampling helper ────────────────

    /// Sample 9 points in a 3×3 grid with the given spacing offset and majority-vote.
    /// Returns Empty if no piece color reaches 2+ votes.
    fn sample_region_vote(&self, frame: &Frame, cx: u32, cy: u32, off: i32) -> CellColor {
        let pts: [(i32, i32); 9] = [
            (-off, -off), (0, -off), (off, -off),
            (-off,    0), (0,    0), (off,    0),
            (-off,  off), (0,  off), (off,  off),
        ];

        let mut counts = [0u8; 9];
        for &(dx, dy) in &pts {
            let x = (cx as i32 + dx).clamp(0, frame.width  as i32 - 1) as u32;
            let y = (cy as i32 + dy).clamp(0, frame.height as i32 - 1) as u32;
            let (r, g, b) = frame.pixel_rgb(x, y);
            let c = self.lut.classify(r, g, b);
            counts[c as usize] = counts[c as usize].saturating_add(1);
        }

        let mut best_color = CellColor::Empty;
        let mut best_count = 0u8;
        for i in 1u8..9 {
            if counts[i as usize] > best_count {
                best_count = counts[i as usize];
                best_color = u8_to_cell_color(i);
            }
        }

        if best_count >= 2 { best_color } else { CellColor::Empty }
    }

    // ── Game active detection ─────────────────

    fn detect_game_active(&self, board: &[[CellColor; 10]; 20]) -> bool {
        // Look for at least 4 non-empty cells — one complete tetromino.
        let mut filled = 0u32;
        for row in board {
            for &cell in row {
                if cell != CellColor::Empty {
                    filled += 1;
                    if filled >= 4 {
                        return true;
                    }
                }
            }
        }
        false
    }
}

// ─────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────

fn frame_pixel_safe(frame: &Frame, x: u32, y: u32) -> (u8, u8, u8) {
    if x < frame.width && y < frame.height {
        frame.pixel_rgb(x, y)
    } else {
        (0, 0, 0)
    }
}

#[inline(always)]
fn u8_to_cell_color(i: u8) -> CellColor {
    match i {
        1 => CellColor::I,
        2 => CellColor::O,
        3 => CellColor::T,
        4 => CellColor::S,
        5 => CellColor::Z,
        6 => CellColor::J,
        7 => CellColor::L,
        8 => CellColor::Garbage,
        _ => CellColor::Empty,
    }
}

// ─────────────────────────────────────────────
//  Color classification (LUT seed function)
// ─────────────────────────────────────────────

/// HSV-based classifier — called only at startup to populate the LUT.
///
/// Key thresholds:
/// - `brightness < 30`  → too dark, Empty
/// - `v < 0.35`         → ghost piece filter: TETR.IO renders the ghost piece at
///                         ~25-35% opacity over a dark background. Real locked
///                         pieces are V > 0.6. Ghost pieces land in V ≈ 0.15-0.40.
///                         Treating them as Empty gives Cold Clear a clean board.
/// - `s < 0.22`         → low saturation = gray = Garbage (if bright) or Empty
/// - hue ranges         → piece identification (calibrated from default TETR.IO skin)
fn classify_color_slow(r: u8, g: u8, b: u8) -> CellColor {
    let brightness = (r as u16 + g as u16 + b as u16) / 3;
    if brightness < 30 {
        return CellColor::Empty;
    }

    let (h, s, v) = rgb_to_hsv(r, g, b);

    // Ghost-piece filter: dim but hue-correct pixels are the ghost overlay,
    // not real locked cells. Filter before the saturation check.
    if v < 0.35 {
        return CellColor::Empty;
    }

    // Low saturation = achromatic = garbage or background
    if s < 0.22 {
        return if brightness > 80 { CellColor::Garbage } else { CellColor::Empty };
    }

    // Hue ranges calibrated from TETR.IO default skin screenshots
    match h as u32 {
        0..=15 | 345..=360 => CellColor::Z,   // Red
        16..=45            => CellColor::L,    // Orange
        46..=75            => CellColor::O,    // Yellow
        76..=150           => CellColor::S,    // Green
        151..=195          => CellColor::I,    // Cyan
        196..=265          => CellColor::J,    // Blue
        266..=344          => CellColor::T,    // Purple
        _                  => CellColor::Empty,
    }
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;

    let max   = r.max(g).max(b);
    let min   = r.min(g).min(b);
    let delta = max - min;

    let v = max;
    let s = if max == 0.0 { 0.0 } else { delta / max };

    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };

    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s, v)
}

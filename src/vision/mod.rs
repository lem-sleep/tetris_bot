//! Board reading / computer vision module.
//!
//! Extracts the full game state from a captured frame by sampling pixel colors
//! at known grid positions. Uses a precomputed RGB→CellColor LUT for O(1)
//! classification, 5-point multi-sample voting per cell for noise tolerance,
//! and a ghost-piece filter so TETR.IO's dim drop-shadow doesn't pollute the board.
//!
//! # Confidence scoring
//! After each board read, a `vision_confidence` value (0.0–1.0) is computed from
//! the fraction of non-empty cells that had strong (≥ 3/5) sample agreement.
//! Values below `config.vision.confidence_threshold` mean calibration is off or
//! the game is not in an expected state — the main loop skips AI submission.

use anyhow::Result;
use crate::capture::Frame;
use crate::config::VisionConfig;

// ─────────────────────────────────────────────
//  Public types
// ─────────────────────────────────────────────

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
    /// 0.0–1.0. Fraction of non-empty board cells where ≥ 3/5 sample points
    /// agreed on the same classification. Low values = calibration problem.
    pub vision_confidence: f32,
}

// ─────────────────────────────────────────────
//  Color LUT
// ─────────────────────────────────────────────

/// Precomputed RGB → CellColor lookup table.
/// Indexed by (r >> 2, g >> 2, b >> 2) → 64×64×64 = 262 144 entries (~256 KB).
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
                    table[idx] = classify_rgb(r, g, b);
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
    confidence_threshold: f32,
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

        // Sentinel pixels: spread across the board to catch any change
        let mut sentinels = vec![
            cell_coords[0  * 10 + 4],   // row 0 col 4 — spawn zone
            cell_coords[0  * 10 + 5],   // row 0 col 5 — spawn zone
            cell_coords[4  * 10 + 4],   // row 4 col 4
            cell_coords[10 * 10 + 5],   // mid board
            cell_coords[15 * 10 + 3],
            cell_coords[19 * 10 + 0],   // bottom-left
            cell_coords[19 * 10 + 9],   // bottom-right
            (cfg.hold_x, cfg.hold_y),
        ];
        if let Some(&(nx, ny)) = cfg.next_positions.first() {
            sentinels.push((nx, ny));
        }

        tracing::info!(
            "BoardReader ready: board at ({},{}) cell_size={} confidence_threshold={:.0}%",
            cfg.board_x, cfg.board_y, cfg.cell_size,
            cfg.confidence_threshold * 100.0
        );

        Ok(Self {
            board_x:  cfg.board_x,
            board_y:  cfg.board_y,
            cell_size: cfg.cell_size,
            hold_x:   cfg.hold_x,
            hold_y:   cfg.hold_y,
            next_positions: cfg.next_positions.clone(),
            confidence_threshold: cfg.confidence_threshold,
            lut,
            cell_coords,
            sentinel_coords:      sentinels,
            last_sentinel_colors: Vec::new(),
        })
    }

    /// Confidence threshold below which we consider the board untrustworthy.
    pub fn confidence_threshold(&self) -> f32 {
        self.confidence_threshold
    }

    // ── Dirty-frame fast path ─────────────────

    pub fn is_frame_dirty(&mut self, frame: &Frame) -> bool {
        let current: Vec<(u8, u8, u8)> = self.sentinel_coords.iter()
            .map(|&(x, y)| pixel_safe(frame, x, y))
            .collect();
        if current == self.last_sentinel_colors {
            return false;
        }
        self.last_sentinel_colors = current;
        true
    }

    // ── Public entry point ────────────────────

    pub fn read_state(&self, frame: &Frame) -> Result<GameState> {
        let (board, vision_confidence) = self.read_board(frame);
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
            vision_confidence,
        })
    }

    // ── Board reading ─────────────────────────

    /// Read all 200 cells via 5-point voting.  Returns board + confidence score.
    ///
    /// Confidence = fraction of non-empty cells where ≥ 3 of 5 samples agreed.
    /// Returns 1.0 on an empty board (nothing to be wrong about).
    fn read_board(&self, frame: &Frame) -> ([[CellColor; 10]; 20], f32) {
        let mut board = [[CellColor::Empty; 10]; 20];
        let mut non_empty = 0u32;
        let mut strong    = 0u32; // non-empty cells with votes >= 3

        for row in 0..20 {
            for col in 0..10 {
                let (cx, cy) = self.cell_coords[row * 10 + col];
                let (color, votes) = self.sample_cell_votes(frame, cx, cy, self.cell_size);
                board[row][col] = color;

                if color != CellColor::Empty {
                    non_empty += 1;
                    if votes >= 3 {
                        strong += 1;
                    } else {
                        // Log uncertain cell so calibration issues show up in the log file
                        let (r, g, b) = frame.pixel_rgb(cx, cy);
                        let (h, s, v) = rgb_to_hsv(r, g, b);
                        tracing::debug!(
                            "Low-confidence cell [{row},{col}]: {:?} \
                             ({votes}/5 votes) RGB({r},{g},{b}) HSV({h:.0},{s:.2},{v:.2})",
                            color,
                        );
                    }
                }
            }
        }

        let confidence = if non_empty == 0 {
            1.0f32
        } else {
            strong as f32 / non_empty as f32
        };

        (board, confidence)
    }

    /// Sample a cell at 5 points (center + NESW at ±cell_size/4) and majority-vote.
    /// Returns (best_color, vote_count).  vote_count 0-5; must be ≥ 2 to classify non-empty.
    fn sample_cell_votes(&self, frame: &Frame, cx: u32, cy: u32, cell_sz: u32) -> (CellColor, u8) {
        let off = (cell_sz / 4).max(4) as i32;
        let pts: [(i32, i32); 5] = [(0, 0), (-off, 0), (off, 0), (0, -off), (0, off)];
        self.vote(frame, cx, cy, &pts, 2)
    }

    // ── Current piece detection ───────────────

    fn detect_current_piece(&self, board: &[[CellColor; 10]; 20]) -> Option<CellColor> {
        // Spawn zone first (rows 0-3) — the active piece always appears here first
        for row in 0..4 {
            for col in 0..10 {
                let c = board[row][col];
                if c != CellColor::Empty && c != CellColor::Garbage {
                    return Some(c);
                }
            }
        }
        // Fallen lower — scan rest of board
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

    fn read_hold_piece(&self, frame: &Frame) -> Option<CellColor> {
        let (c, _) = self.sample_region(frame, self.hold_x, self.hold_y, 8);
        if c != CellColor::Empty { Some(c) } else { None }
    }

    // ── Next queue reading ────────────────────

    fn read_next_queue(&self, frame: &Frame) -> Vec<CellColor> {
        self.next_positions.iter()
            .filter_map(|&(x, y)| {
                let (c, _) = self.sample_region(frame, x, y, 8);
                if c != CellColor::Empty { Some(c) } else { None }
            })
            .collect()
    }

    // ── Shared multi-point sampling ───────────

    /// Sample a 3×3 grid at the given offset spacing and return (best_color, vote_count).
    fn sample_region(&self, frame: &Frame, cx: u32, cy: u32, off: i32) -> (CellColor, u8) {
        let pts: [(i32, i32); 9] = [
            (-off, -off), (0, -off), (off, -off),
            (-off,    0), (0,    0), (off,    0),
            (-off,  off), (0,  off), (off,  off),
        ];
        self.vote(frame, cx, cy, &pts, 2)
    }

    /// Core voting function: sample each offset point, tally CellColor votes,
    /// return (winning_non_empty_color, its_vote_count) or (Empty, empty_count)
    /// if no non-empty color reaches `min_votes`.
    fn vote(
        &self,
        frame: &Frame,
        cx: u32, cy: u32,
        offsets: &[(i32, i32)],
        min_votes: u8,
    ) -> (CellColor, u8) {
        let mut counts = [0u8; 9]; // index = CellColor as u8
        for &(dx, dy) in offsets {
            let x = (cx as i32 + dx).clamp(0, frame.width  as i32 - 1) as u32;
            let y = (cy as i32 + dy).clamp(0, frame.height as i32 - 1) as u32;
            let (r, g, b) = frame.pixel_rgb(x, y);
            let c = self.lut.classify(r, g, b);
            counts[c as usize] = counts[c as usize].saturating_add(1);
        }

        let mut best_color = CellColor::Empty;
        let mut best_count = 0u8;
        for i in 1u8..9 { // skip 0 = Empty
            if counts[i as usize] > best_count {
                best_count = counts[i as usize];
                best_color = u8_to_cell_color(i);
            }
        }

        if best_count >= min_votes {
            (best_color, best_count)
        } else {
            (CellColor::Empty, counts[0])
        }
    }

    // ── Game active detection ─────────────────

    fn detect_game_active(&self, board: &[[CellColor; 10]; 20]) -> bool {
        let mut filled = 0u32;
        for row in board {
            for &cell in row {
                if cell != CellColor::Empty {
                    filled += 1;
                    if filled >= 4 { return true; }
                }
            }
        }
        false
    }
}

// ─────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────

fn pixel_safe(frame: &Frame, x: u32, y: u32) -> (u8, u8, u8) {
    if x < frame.width && y < frame.height { frame.pixel_rgb(x, y) } else { (0, 0, 0) }
}

#[inline(always)]
fn u8_to_cell_color(i: u8) -> CellColor {
    match i {
        1 => CellColor::I, 2 => CellColor::O, 3 => CellColor::T,
        4 => CellColor::S, 5 => CellColor::Z, 6 => CellColor::J,
        7 => CellColor::L, 8 => CellColor::Garbage,
        _ => CellColor::Empty,
    }
}

// ─────────────────────────────────────────────
//  Color classification
// ─────────────────────────────────────────────

/// Classify a single RGB pixel to a CellColor.
///
/// This function is called only at startup to populate the LUT.  After that
/// all classification goes through the O(1) LUT lookup.
///
/// # Thresholds (calibrated against TETR.IO default skin, 1920×1080 BW)
///
/// | Filter           | Threshold | Reason                                       |
/// |------------------|-----------|----------------------------------------------|
/// | brightness < 30  | → Empty   | Truly dark background pixels                 |
/// | HSV value < 0.35 | → Empty   | Ghost-piece filter: TETR.IO renders the drop |
/// |                  |           | shadow at ~25-35% opacity.  Real locked       |
/// |                  |           | pieces are V > 0.60; ghost pieces ≈ 0.15-0.40|
/// | saturation < 0.22| → Garbage | Low-chroma = gray/white = garbage or border   |
/// |                  |    or Empty|                                               |
pub fn classify_rgb(r: u8, g: u8, b: u8) -> CellColor {
    let brightness = (r as u16 + g as u16 + b as u16) / 3;
    if brightness < 30 {
        return CellColor::Empty;
    }

    let (h, s, v) = rgb_to_hsv(r, g, b);

    // Ghost-piece filter: dim hue-correct pixels are the ghost overlay,
    // not real locked cells.  Must come before the saturation check.
    if v < 0.35 {
        return CellColor::Empty;
    }

    // Low saturation = achromatic = garbage lines or UI background
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

pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
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

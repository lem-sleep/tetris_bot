//! Board reading / computer vision module.
//!
//! Extracts the full game state from a captured frame by sampling pixel colors
//! at known grid positions. Uses a precomputed RGB→CellColor lookup table for
//! O(1) classification instead of per-pixel HSV float math.

use anyhow::Result;
use crate::capture::Frame;
use crate::config::VisionConfig;

/// The 7 standard Tetris pieces + empty + garbage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CellColor {
    Empty = 0,
    I = 1,
    O = 2,
    T = 3,
    S = 4,
    Z = 5,
    J = 6,
    L = 7,
    Garbage = 8,
}

/// Full game state extracted from a single frame.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GameState {
    pub board: [[CellColor; 10]; 20],
    pub current_piece: Option<CellColor>,
    pub hold_piece: Option<CellColor>,
    pub next_queue: Vec<CellColor>,
    pub is_active: bool,
    pub incoming_garbage: u32,
}

/// Precomputed RGB → CellColor lookup table.
/// Indexed by (r >> 2, g >> 2, b >> 2) → 64×64×64 = 262,144 entries.
/// Fits in ~256KB — well within L2 cache.
struct ColorLut {
    table: Vec<CellColor>,
}

impl ColorLut {
    fn new() -> Self {
        let mut table = vec![CellColor::Empty; 64 * 64 * 64];
        for ri in 0..64u32 {
            for gi in 0..64u32 {
                for bi in 0..64u32 {
                    // Map back to approximate RGB (center of the 4-value bin)
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
        let idx = ((r as usize) >> 2) * 4096 + ((g as usize) >> 2) * 64 + ((b as usize) >> 2);
        unsafe { *self.table.get_unchecked(idx) }
    }
}

/// Reads game state from captured frames.
#[allow(dead_code)]
pub struct BoardReader {
    board_x: u32,
    board_y: u32,
    cell_size: u32,
    hold_x: u32,
    hold_y: u32,
    next_positions: Vec<(u32, u32)>,
    lut: ColorLut,
    /// Precomputed pixel coordinates for each board cell center (row, col) → (px, py).
    cell_coords: [(u32, u32); 200],
    /// Sentinel pixels for fast dirty detection — sample a few key positions.
    /// If these haven't changed, skip the full board read.
    sentinel_coords: Vec<(u32, u32)>,
    last_sentinel_colors: Vec<(u8, u8, u8)>,
}

impl BoardReader {
    pub fn new(cfg: &VisionConfig) -> Result<Self> {
        let lut = ColorLut::new();

        // Precompute all 200 cell center coordinates
        let mut cell_coords = [(0u32, 0u32); 200];
        for row in 0..20u32 {
            for col in 0..10u32 {
                let px = cfg.board_x + col * cfg.cell_size + cfg.cell_size / 2;
                let py = cfg.board_y + row * cfg.cell_size + cfg.cell_size / 2;
                cell_coords[(row * 10 + col) as usize] = (px, py);
            }
        }

        // Sentinel pixels: sample corners + center + next queue first slot.
        // These change when pieces lock, lines clear, or a new piece spawns.
        let sentinels = vec![
            cell_coords[4 * 10 + 5],   // row 4, col 5 (spawn area center)
            cell_coords[0 * 10 + 4],   // row 0, col 4 (top center)
            cell_coords[19 * 10 + 0],  // row 19, col 0 (bottom-left)
            cell_coords[19 * 10 + 9],  // row 19, col 9 (bottom-right)
            cell_coords[10 * 10 + 5],  // row 10, col 5 (mid center)
            cell_coords[15 * 10 + 3],  // row 15, col 3 (lower area)
            (cfg.hold_x, cfg.hold_y),  // hold piece
        ];
        // Add first next queue position if available
        let mut all_sentinels = sentinels;
        if let Some(&(nx, ny)) = cfg.next_positions.first() {
            all_sentinels.push((nx, ny));
        }

        Ok(Self {
            board_x: cfg.board_x,
            board_y: cfg.board_y,
            cell_size: cfg.cell_size,
            hold_x: cfg.hold_x,
            hold_y: cfg.hold_y,
            next_positions: cfg.next_positions.clone(),
            lut,
            cell_coords,
            sentinel_coords: all_sentinels,
            last_sentinel_colors: Vec::new(),
        })
    }

    /// Quick check: have the sentinel pixels changed since last frame?
    /// Returns true if the frame looks different (needs full read).
    pub fn is_frame_dirty(&mut self, frame: &Frame) -> bool {
        let current: Vec<(u8, u8, u8)> = self.sentinel_coords.iter()
            .map(|&(x, y)| {
                if x < frame.width && y < frame.height {
                    frame.pixel_rgb(x, y)
                } else {
                    (0, 0, 0)
                }
            })
            .collect();

        if current == self.last_sentinel_colors {
            return false;
        }
        self.last_sentinel_colors = current;
        true
    }

    /// Read the full game state from a captured frame.
    pub fn read_state(&self, frame: &Frame) -> Result<GameState> {
        let board = self.read_board(frame);
        let current_piece = self.detect_current_piece(&board);
        let hold_piece = self.read_single_piece(frame, self.hold_x, self.hold_y);
        let next_queue = self.read_next_queue(frame);
        let is_active = self.detect_game_active(&board);

        Ok(GameState {
            board,
            current_piece,
            hold_piece,
            next_queue,
            is_active,
            incoming_garbage: 0,
        })
    }

    /// Sample the 10x20 board grid using precomputed coordinates + LUT.
    fn read_board(&self, frame: &Frame) -> [[CellColor; 10]; 20] {
        let mut board = [[CellColor::Empty; 10]; 20];
        for row in 0..20 {
            for col in 0..10 {
                let (px, py) = self.cell_coords[row * 10 + col];
                if px < frame.width && py < frame.height {
                    let (r, g, b) = frame.pixel_rgb(px, py);
                    board[row][col] = self.lut.classify(r, g, b);
                }
            }
        }
        board
    }

    fn detect_current_piece(&self, board: &[[CellColor; 10]; 20]) -> Option<CellColor> {
        for row in 0..10 {
            for col in 0..10 {
                let c = board[row][col];
                if c != CellColor::Empty && c != CellColor::Garbage {
                    return Some(c);
                }
            }
        }
        None
    }

    fn read_single_piece(&self, frame: &Frame, x: u32, y: u32) -> Option<CellColor> {
        if x < frame.width && y < frame.height {
            let (r, g, b) = frame.pixel_rgb(x, y);
            let color = self.lut.classify(r, g, b);
            if color != CellColor::Empty {
                return Some(color);
            }
        }
        None
    }

    fn read_next_queue(&self, frame: &Frame) -> Vec<CellColor> {
        self.next_positions
            .iter()
            .filter_map(|&(x, y)| self.read_single_piece(frame, x, y))
            .collect()
    }

    fn detect_game_active(&self, board: &[[CellColor; 10]; 20]) -> bool {
        for row in 16..20 {
            for col in 0..10 {
                if board[row][col] != CellColor::Empty {
                    return true;
                }
            }
        }
        false
    }
}

/// Original HSV-based classification — used only at startup to build the LUT.
fn classify_color_slow(r: u8, g: u8, b: u8) -> CellColor {
    let brightness = (r as u16 + g as u16 + b as u16) / 3;
    if brightness < 30 {
        return CellColor::Empty;
    }

    let (h, s, _v) = rgb_to_hsv(r, g, b);

    if s < 0.20 {
        return if brightness > 70 { CellColor::Garbage } else { CellColor::Empty };
    }

    match h as u32 {
        0..=15 | 345..=360 => CellColor::Z,
        16..=45 => CellColor::L,
        46..=75 => CellColor::O,
        76..=150 => CellColor::S,
        151..=195 => CellColor::I,
        196..=265 => CellColor::J,
        266..=344 => CellColor::T,
        _ => CellColor::Empty,
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

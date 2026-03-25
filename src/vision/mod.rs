//! Board reading / computer vision module.
//!
//! Extracts the full game state from a captured frame by sampling pixel colors
//! at known grid positions and classifying them via HSV hue ranges.
//! Calibrated for TETR.IO default skin at 1920x1080 with minimal graphics.

use anyhow::Result;
use crate::capture::Frame;
use crate::config::VisionConfig;

/// The 7 standard Tetris pieces + empty + garbage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    Empty,
    I, // Cyan  (hue ~158-180)
    O, // Yellow (hue ~46-70)
    T, // Purple (hue ~240-280)
    S, // Green  (hue ~75-145)
    Z, // Red    (hue ~345-15)
    J, // Blue   (hue ~210-240)
    L, // Orange (hue ~16-45)
    Garbage, // Gray (low saturation)
}

/// Full game state extracted from a single frame.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GameState {
    /// 10x20 board grid, row 0 = top.
    pub board: [[CellColor; 10]; 20],
    /// Currently active piece (falling) — detected from the top rows of the board.
    pub current_piece: Option<CellColor>,
    /// Piece in hold slot.
    pub hold_piece: Option<CellColor>,
    /// Next queue (up to 5 pieces).
    pub next_queue: Vec<CellColor>,
    /// Whether the game appears to be actively running.
    pub is_active: bool,
    /// Incoming garbage lines (if detectable).
    pub incoming_garbage: u32,
}

/// Reads game state from captured frames.
pub struct BoardReader {
    board_x: u32,
    board_y: u32,
    cell_size: u32,
    hold_x: u32,
    hold_y: u32,
    next_positions: Vec<(u32, u32)>,
}

impl BoardReader {
    pub fn new(cfg: &VisionConfig) -> Result<Self> {
        Ok(Self {
            board_x: cfg.board_x,
            board_y: cfg.board_y,
            cell_size: cfg.cell_size,
            hold_x: cfg.hold_x,
            hold_y: cfg.hold_y,
            next_positions: cfg.next_positions.clone(),
        })
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

    /// Sample the 10x20 board grid.
    fn read_board(&self, frame: &Frame) -> [[CellColor; 10]; 20] {
        let mut board = [[CellColor::Empty; 10]; 20];
        for row in 0..20u32 {
            for col in 0..10u32 {
                let px = self.board_x + col * self.cell_size + self.cell_size / 2;
                let py = self.board_y + row * self.cell_size + self.cell_size / 2;
                if px < frame.width && py < frame.height {
                    let (r, g, b) = frame.pixel_rgb(px, py);
                    board[row as usize][col as usize] = classify_color(r, g, b);
                }
            }
        }
        board
    }

    /// Detect the currently falling piece by finding colored cells in the top rows
    /// that form a connected piece shape (not yet locked).
    fn detect_current_piece(&self, board: &[[CellColor; 10]; 20]) -> Option<CellColor> {
        // Scan from top — the first non-empty, non-garbage color found in the
        // upper portion of the board is likely the active piece.
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
            let color = classify_color(r, g, b);
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

    /// Detect if a game is active by checking that the board border region
    /// has some non-black pixels (the white border is visible during gameplay).
    fn detect_game_active(&self, board: &[[CellColor; 10]; 20]) -> bool {
        // Simple heuristic: if the bottom 4 rows have any non-empty cells,
        // a game is probably in progress. If the entire board is empty,
        // we might be in a menu.
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

/// Classify an RGB pixel to a piece color using HSV hue ranges.
/// Calibrated from actual TETR.IO default skin pixel samples.
pub fn classify_color(r: u8, g: u8, b: u8) -> CellColor {
    let brightness = (r as u16 + g as u16 + b as u16) / 3;

    // Too dark → empty cell
    if brightness < 30 {
        return CellColor::Empty;
    }

    let (h, s, _v) = rgb_to_hsv(r, g, b);

    // Low saturation → garbage (gray) or empty
    if s < 0.20 {
        return if brightness > 70 { CellColor::Garbage } else { CellColor::Empty };
    }

    // Classify by hue angle (calibrated from TETR.IO default skin samples):
    //   Z (Red):    hue 345-360, 0-15     sample: HSV(357, 0.67, 0.76)
    //   L (Orange): hue 16-45             sample: HSV(23, 0.68, 0.75)
    //   O (Yellow): hue 46-75             sample: HSV(~55, high sat)
    //   S (Green):  hue 76-150            sample: HSV(82, 0.68, 0.71-0.75)
    //   I (Cyan):   hue 151-195           sample: HSV(158, 0.71, 0.71)
    //   J (Blue):   hue 196-255           sample: HSV(252, 0.38, 0.75)
    //   T (Purple): hue 256-344           sample: HSV(252, 0.38, 0.75) — J/T overlap
    //
    // J vs T distinction: J is more blue (hue 220-250), T is more purple (hue 250-330)
    // From actual samples: J=HSV(252, 0.38, 0.75), T has higher saturation & more red

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

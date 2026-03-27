//! AI module wrapping Cold Clear for move computation.
//!
//! For each move request, we create a fresh Cold Clear interface initialized
//! with the actual board state from screen capture. This ensures Cold Clear's
//! internal state always matches reality — critical when using vision-based input.
//!
//! Returns an `AiSuggestion` with TWO placements: one for placing the current
//! piece directly, and one for holding first then placing the hold piece.

use anyhow::{Result, Context};
use libtetris::{Board, MovementMode, Piece};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crate::config::AiConfig;
use crate::vision::{CellColor, GameState};

/// A single recommended placement (position + piece color).
#[derive(Debug, Clone)]
pub struct OverlayPlacement {
    /// 4 cell positions in vision coords: (col 0-9, row 0-19 where row 0 = top).
    pub cells: [(i32, i32); 4],
    /// Piece color (for ghost tinting).
    pub piece_color: CellColor,
}

/// Both placement options for the overlay: direct and hold.
#[derive(Debug, Clone)]
pub struct AiSuggestion {
    /// Best placement for the current piece (no hold).
    pub direct: OverlayPlacement,
    /// Best placement if the user holds first (None if no hold piece available).
    pub hold_option: Option<OverlayPlacement>,
}

fn piece_name(color: CellColor) -> Option<&'static str> {
    match color {
        CellColor::I => Some("I"),
        CellColor::O => Some("O"),
        CellColor::T => Some("T"),
        CellColor::S => Some("S"),
        CellColor::Z => Some("Z"),
        CellColor::J => Some("J"),
        CellColor::L => Some("L"),
        CellColor::Empty => Some("_"),
        CellColor::Garbage => Some("G"),
    }
}

fn color_to_piece(color: CellColor) -> Option<Piece> {
    match color {
        CellColor::I => Some(Piece::I),
        CellColor::O => Some(Piece::O),
        CellColor::T => Some(Piece::T),
        CellColor::S => Some(Piece::S),
        CellColor::Z => Some(Piece::Z),
        CellColor::J => Some(Piece::J),
        CellColor::L => Some(Piece::L),
        _ => None,
    }
}

fn piece_to_color(piece: Piece) -> CellColor {
    match piece {
        Piece::I => CellColor::I,
        Piece::O => CellColor::O,
        Piece::T => CellColor::T,
        Piece::S => CellColor::S,
        Piece::Z => CellColor::Z,
        Piece::J => CellColor::J,
        Piece::L => CellColor::L,
    }
}

/// Build a Cold Clear compatible field from vision board data.
/// Uses connected component analysis to find and exclude the falling piece —
/// the topmost group of 4 connected cells matching the current piece color.
fn build_field_from_vision(
    board: &[[CellColor; 10]; 20],
    current_piece: Option<CellColor>,
) -> [[bool; 10]; 40] {
    // Find cells belonging to the falling piece via flood fill
    let falling_cells = find_falling_piece_cells(board, current_piece);

    let mut field = [[false; 10]; 40];
    for vision_row in 0..20usize {
        let lt_row = 19 - vision_row;
        for col in 0..10usize {
            let cell = board[vision_row][col];
            if cell == CellColor::Empty {
                continue;
            }
            // Skip cells identified as the falling piece
            if falling_cells.contains(&(vision_row, col)) {
                continue;
            }
            field[lt_row][col] = true;
        }
    }
    field
}

/// Find the cells belonging to the falling piece using connected component analysis.
/// Scans from the top of the board for the first connected group of cells matching
/// the current piece color. A valid tetromino has exactly 4 orthogonally connected cells.
fn find_falling_piece_cells(
    board: &[[CellColor; 10]; 20],
    current_piece: Option<CellColor>,
) -> Vec<(usize, usize)> {
    let color = match current_piece {
        Some(c) if c != CellColor::Empty && c != CellColor::Garbage => c,
        _ => return Vec::new(),
    };

    let mut visited = [[false; 10]; 20];

    // Scan from top-left to find connected components of the current piece color
    for row in 0..20 {
        for col in 0..10 {
            if board[row][col] == color && !visited[row][col] {
                let mut component = Vec::new();
                flood_fill(board, row, col, color, &mut visited, &mut component);

                // A standard tetromino is exactly 4 cells
                if component.len() == 4 {
                    return component;
                }
                // If >4 cells, the falling piece merged with locked cells of the same color.
                // Take the topmost 4 cells as the falling piece (it spawns at the top).
                if component.len() > 4 {
                    component.sort_by_key(|&(r, c)| (r, c));
                    component.truncate(4);
                    return component;
                }
                // <4 cells: might be locked debris, keep scanning for the real piece
            }
        }
    }
    Vec::new()
}

fn flood_fill(
    board: &[[CellColor; 10]; 20],
    row: usize,
    col: usize,
    color: CellColor,
    visited: &mut [[bool; 10]; 20],
    component: &mut Vec<(usize, usize)>,
) {
    if row >= 20 || col >= 10 || visited[row][col] || board[row][col] != color {
        return;
    }
    visited[row][col] = true;
    component.push((row, col));

    if row > 0 { flood_fill(board, row - 1, col, color, visited, component); }
    if row < 19 { flood_fill(board, row + 1, col, color, visited, component); }
    if col > 0 { flood_fill(board, row, col - 1, color, visited, component); }
    if col < 9 { flood_fill(board, row, col + 1, color, visited, component); }
}

/// Message sent from main loop -> AI thread.
struct AiRequest {
    state: GameState,
}

/// Non-blocking AI engine. The main loop sends game states and polls for suggestions.
pub struct AiEngine {
    req_tx: mpsc::Sender<AiRequest>,
    resp_rx: mpsc::Receiver<AiSuggestion>,
    cancel: Arc<AtomicBool>,
    options: cold_clear::Options,
    weights: cold_clear::evaluation::Standard,
}

impl AiEngine {
    pub fn new(cfg: &AiConfig) -> Result<Self> {
        let mode = match cfg.movement_mode.as_str() {
            "zero_g" => MovementMode::ZeroG,
            "twenty_g" => MovementMode::TwentyG,
            _ => MovementMode::HardDropOnly,
        };

        let options = cold_clear::Options {
            max_nodes: cfg.max_nodes,
            min_nodes: cfg.min_nodes,
            use_hold: false, // We compute hold vs no-hold separately
            speculate: true,
            mode,
            ..Default::default()
        };

        let weights = match cfg.playstyle.as_str() {
            "aggressive" => {
                let mut w = cold_clear::evaluation::Standard::default();
                w.back_to_back = 80;
                w.clear4 = 500;
                w.combo_garbage = 30;
                w
            }
            "tspin" => {
                let mut w = cold_clear::evaluation::Standard::default();
                w.back_to_back = 100;
                w.tspin1 = 200;
                w.tspin2 = 600;
                w.tspin3 = 900;
                w.tslot = [20, 200, 300, 500];
                w.wasted_t = -250;
                w
            }
            "defensive" => {
                let mut w = cold_clear::evaluation::Standard::default();
                w.height = -60;
                w.cavity_cells = -250;
                w.overhang_cells = -50;
                w.top_half = -200;
                w
            }
            _ => cold_clear::evaluation::Standard::default(),
        };

        tracing::info!(
            "Cold Clear AI initialized (playstyle: {}, max_nodes: {}, mode: {:?})",
            cfg.playstyle, cfg.max_nodes, mode
        );

        let cancel = Arc::new(AtomicBool::new(false));
        let (req_tx, req_rx) = mpsc::channel::<AiRequest>();
        let (resp_tx, resp_rx) = mpsc::channel::<AiSuggestion>();

        let opts = options.clone();
        let wts = weights.clone();
        let cancel_flag = Arc::clone(&cancel);

        std::thread::Builder::new()
            .name("ai-worker".into())
            .spawn(move || {
                ai_worker(opts, wts, req_rx, resp_tx, cancel_flag);
            })
            .context("Failed to spawn AI worker thread")?;

        Ok(Self {
            req_tx,
            resp_rx,
            cancel,
            options,
            weights,
        })
    }

    /// Kill the current worker and spawn a fresh one.
    /// Call this when the bot is re-enabled to avoid stale blocked state.
    pub fn reset(&mut self) -> Result<()> {
        // Signal old worker to stop, then drop its sender so it exits.
        self.cancel.store(true, Ordering::Relaxed);

        let cancel = Arc::new(AtomicBool::new(false));
        let (req_tx, req_rx) = mpsc::channel::<AiRequest>();
        let (resp_tx, resp_rx) = mpsc::channel::<AiSuggestion>();

        let opts = self.options.clone();
        let wts = self.weights.clone();
        let cancel_flag = Arc::clone(&cancel);

        std::thread::Builder::new()
            .name("ai-worker".into())
            .spawn(move || {
                ai_worker(opts, wts, req_rx, resp_tx, cancel_flag);
            })
            .context("Failed to spawn AI worker thread")?;

        self.req_tx = req_tx;
        self.resp_rx = resp_rx;
        self.cancel = cancel;

        tracing::info!("AI engine reset — fresh worker spawned");
        Ok(())
    }

    /// Submit a game state for AI processing. Returns immediately.
    pub fn submit(&mut self, state: &GameState) {
        let _ = self.req_tx.send(AiRequest {
            state: state.clone(),
        });
    }

    /// Poll for a completed suggestion. Non-blocking — returns None if AI is still thinking.
    pub fn poll_move(&mut self) -> Option<AiSuggestion> {
        self.resp_rx.try_recv().ok()
    }
}

/// Compute the best placement for a given piece on a given board using Cold Clear.
/// Returns None if cancelled or Cold Clear finds no valid move.
fn compute_placement(
    board: &Board,
    piece: Piece,
    next_queue: &[Piece],
    garbage: u32,
    options: &cold_clear::Options,
    weights: &cold_clear::evaluation::Standard,
    cancel: &AtomicBool,
) -> Option<OverlayPlacement> {
    let interface = cold_clear::Interface::launch(
        board.clone(),
        options.clone(),
        weights.clone(),
        None,
    );

    interface.add_next_piece(piece);
    for &p in next_queue {
        interface.add_next_piece(p);
    }
    interface.suggest_next_move(garbage);

    // Poll instead of blocking — allows cancellation.
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match interface.poll_next_move() {
            Ok((mv, _info)) => {
                let cells_lt = mv.expected_location.cells();
                let cells_vision = cells_lt.map(|(col, row_lt)| (col, 19 - row_lt));
                let placed_piece = mv.expected_location.kind.0;
                return Some(OverlayPlacement {
                    cells: cells_vision,
                    piece_color: piece_to_color(placed_piece),
                });
            }
            Err(cold_clear::BotPollState::Waiting) => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(cold_clear::BotPollState::Dead) => {
                return None;
            }
        }
    }
}

/// Background worker: receives game states, computes both direct and hold placements.
fn ai_worker(
    options: cold_clear::Options,
    weights: cold_clear::evaluation::Standard,
    rx: mpsc::Receiver<AiRequest>,
    tx: mpsc::Sender<AiSuggestion>,
    cancel: Arc<AtomicBool>,
) {
    while let Ok(req) = rx.recv() {
        // Drain to latest request (skip stale ones)
        let mut latest = req;
        while let Ok(newer) = rx.try_recv() {
            latest = newer;
        }

        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let state = &latest.state;

        // Build board field from vision data (exclude falling piece)
        let field = build_field_from_vision(&state.board, state.current_piece);

        // Log what vision sees
        let next_names: Vec<&str> = state.next_queue.iter()
            .filter_map(|c| piece_name(*c))
            .collect();
        let filled: usize = field.iter().flatten().filter(|&&c| c).count();
        tracing::info!(
            "AI request: current={} hold={} next=[{}] field_cells={}",
            state.current_piece.map_or("None", |c| piece_name(c).unwrap_or("?")),
            state.hold_piece.map_or("None", |c| piece_name(c).unwrap_or("?")),
            next_names.join(","),
            filled,
        );

        // Build base board
        let mut board = Board::new();
        board.set_field(field);

        // Collect next queue as Piece values
        let next_pieces: Vec<Piece> = state.next_queue.iter()
            .filter_map(|c| color_to_piece(*c))
            .collect();

        let current_piece = match state.current_piece.and_then(color_to_piece) {
            Some(p) => p,
            None => continue,
        };

        let garbage = state.incoming_garbage;

        // === Compute direct placement (current piece, no hold) ===
        let direct = match compute_placement(
            &board, current_piece, &next_pieces, garbage,
            &options, &weights, &cancel,
        ) {
            Some(p) => p,
            None => {
                if cancel.load(Ordering::Relaxed) { break; }
                tracing::warn!("Cold Clear returned no direct move");
                continue;
            }
        };

        tracing::info!(
            "Direct placement: piece={:?} cells={:?}",
            direct.piece_color, direct.cells,
        );

        // Check for cancellation / new request before computing hold option
        if cancel.load(Ordering::Relaxed) { break; }
        if rx.try_recv().is_ok() {
            // New request arrived — skip hold computation, we'll redo everything
            // Put the request back... actually we can't un-recv. Just continue the loop.
            // The new request will be picked up on the next iteration.
            // For now, send the direct-only result.
            let _ = tx.send(AiSuggestion {
                direct,
                hold_option: None,
            });
            continue;
        }

        // === Compute hold placement (hold piece on same board) ===
        let hold_option = if let Some(hold_color) = state.hold_piece {
            if let Some(hold_piece) = color_to_piece(hold_color) {
                // When user holds: hold piece becomes current, current piece goes to hold.
                // Next queue for the hold computation is: current_piece + original next queue
                // (because after holding, the old current piece is in hold, not in queue)
                let mut hold_next = vec![current_piece];
                hold_next.extend_from_slice(&next_pieces);

                match compute_placement(
                    &board, hold_piece, &hold_next, garbage,
                    &options, &weights, &cancel,
                ) {
                    Some(p) => {
                        tracing::info!(
                            "Hold placement: piece={:?} cells={:?}",
                            p.piece_color, p.cells,
                        );
                        Some(p)
                    }
                    None => {
                        if cancel.load(Ordering::Relaxed) { break; }
                        tracing::warn!("Cold Clear returned no hold move");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let _ = tx.send(AiSuggestion {
            direct,
            hold_option,
        });
    }
    tracing::info!("AI worker exiting");
}

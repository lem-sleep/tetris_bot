//! AI module wrapping Cold Clear for move computation.
//!
//! Cold Clear runs on its own background threads. We communicate with it via
//! channels so the main capture loop is never blocked waiting for a move.

use anyhow::{Result, Context};
use libtetris::{Board, Piece, PieceMovement};
use std::sync::mpsc;
use crate::config::AiConfig;
use crate::vision::{CellColor, GameState};

/// A computed move: the sequence of inputs to place the current piece.
#[derive(Debug, Clone)]
pub struct AiMove {
    pub inputs: Vec<MoveInput>,
    /// Board height at time of move request — used for dynamic PPS.
    pub board_height: u8,
}

/// Individual input actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum MoveInput {
    Left,
    Right,
    RotateCW,
    RotateCCW,
    Rotate180,
    SoftDrop,
    HardDrop,
    Hold,
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

fn cc_movements_to_inputs(movements: &[PieceMovement], hold: bool) -> Vec<MoveInput> {
    let mut inputs = Vec::with_capacity(movements.len() + 2);
    if hold {
        inputs.push(MoveInput::Hold);
    }
    for m in movements {
        match m {
            PieceMovement::Left => inputs.push(MoveInput::Left),
            PieceMovement::Right => inputs.push(MoveInput::Right),
            PieceMovement::Cw => inputs.push(MoveInput::RotateCW),
            PieceMovement::Ccw => inputs.push(MoveInput::RotateCCW),
            PieceMovement::SonicDrop => inputs.push(MoveInput::SoftDrop),
        }
    }
    inputs.push(MoveInput::HardDrop);
    inputs
}

/// Compute the max occupied row height (0 = empty board, 20 = topped out).
fn board_height(board: &[[CellColor; 10]; 20]) -> u8 {
    for row in 0..20 {
        for col in 0..10 {
            if board[row][col] != CellColor::Empty {
                return (20 - row) as u8;
            }
        }
    }
    0
}

/// A compact signature of the game state for dedup — avoids re-requesting
/// moves when the board + queue haven't actually changed.
#[derive(Clone, PartialEq, Eq)]
struct StateSignature {
    current: Option<CellColor>,
    queue: [Option<CellColor>; 5],
    board_hash: u64,
}

impl StateSignature {
    fn from_game_state(state: &GameState) -> Self {
        let mut queue = [None; 5];
        for (i, c) in state.next_queue.iter().take(5).enumerate() {
            queue[i] = Some(*c);
        }
        // FNV-1a hash of the board — fast, no crypto needed
        let mut h: u64 = 0xcbf29ce484222325;
        for row in &state.board {
            for cell in row {
                h ^= *cell as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
        Self {
            current: state.current_piece,
            queue,
            board_hash: h,
        }
    }
}

/// Message sent from main loop → AI thread.
struct AiRequest {
    state: GameState,
    height: u8,
}

/// Non-blocking AI engine. The main loop sends game states and polls for moves.
pub struct AiEngine {
    req_tx: mpsc::Sender<AiRequest>,
    resp_rx: mpsc::Receiver<AiMove>,
    last_sig: Option<StateSignature>,
    pending: bool,
    cached_move: Option<AiMove>,
}

impl AiEngine {
    pub fn new(cfg: &AiConfig) -> Result<Self> {
        let options = cold_clear::Options {
            max_nodes: cfg.max_nodes,
            min_nodes: cfg.min_nodes,
            use_hold: true,
            speculate: true,
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

        let board = Board::new();
        let interface = cold_clear::Interface::launch(board, options, weights, None);
        tracing::info!("Cold Clear AI initialized (playstyle: {}, max_nodes: {})", cfg.playstyle, cfg.max_nodes);

        let (req_tx, req_rx) = mpsc::channel::<AiRequest>();
        let (resp_tx, resp_rx) = mpsc::channel::<AiMove>();

        // Spawn dedicated AI worker thread
        std::thread::Builder::new()
            .name("ai-worker".into())
            .spawn(move || {
                ai_worker(interface, req_rx, resp_tx);
            })
            .context("Failed to spawn AI worker thread")?;

        Ok(Self {
            req_tx,
            resp_rx,
            last_sig: None,
            pending: false,
            cached_move: None,
        })
    }

    /// Submit a game state for AI processing. Returns immediately.
    /// Skips submission if the state hasn't meaningfully changed.
    pub fn submit(&mut self, state: &GameState) {
        let sig = StateSignature::from_game_state(state);
        if self.pending {
            // Already computing — only resubmit if state changed
            if self.last_sig.as_ref() == Some(&sig) {
                return;
            }
        }
        if self.last_sig.as_ref() == Some(&sig) && self.cached_move.is_some() {
            return; // Same state, already have a cached move
        }

        let height = board_height(&state.board);
        self.last_sig = Some(sig);
        self.pending = true;

        let _ = self.req_tx.send(AiRequest {
            state: state.clone(),
            height,
        });
    }

    /// Poll for a completed move. Non-blocking — returns None if AI is still thinking.
    pub fn poll_move(&mut self) -> Option<AiMove> {
        match self.resp_rx.try_recv() {
            Ok(mv) => {
                self.pending = false;
                self.cached_move = Some(mv.clone());
                Some(mv)
            }
            Err(_) => None,
        }
    }

}

/// Background worker: receives game states, feeds Cold Clear, returns moves.
fn ai_worker(
    interface: cold_clear::Interface,
    rx: mpsc::Receiver<AiRequest>,
    tx: mpsc::Sender<AiMove>,
) {
    let mut pieces_fed: usize = 0;

    while let Ok(req) = rx.recv() {
        // Drain to latest request (skip stale ones)
        let mut latest = req;
        while let Ok(newer) = rx.try_recv() {
            latest = newer;
        }

        let state = &latest.state;

        // Build piece list: current + queue
        let mut pieces: Vec<Piece> = Vec::with_capacity(6);
        if let Some(current) = state.current_piece {
            if let Some(p) = color_to_piece(current) {
                pieces.push(p);
            }
        }
        for color in &state.next_queue {
            if let Some(p) = color_to_piece(*color) {
                pieces.push(p);
            }
        }

        // Feed only new pieces
        for piece in pieces.iter().skip(pieces_fed) {
            interface.add_next_piece(*piece);
        }
        pieces_fed = pieces_fed.max(pieces.len());

        // Request + block (this thread is dedicated, so blocking is fine)
        interface.suggest_next_move(state.incoming_garbage);

        if let Some((mv, _info)) = interface.block_next_move() {
            interface.play_next_move(mv.expected_location);
            let inputs = cc_movements_to_inputs(&mv.inputs, mv.hold);
            let _ = tx.send(AiMove {
                inputs,
                board_height: latest.height,
            });
        } else {
            tracing::warn!("Cold Clear returned no move");
        }
    }
}

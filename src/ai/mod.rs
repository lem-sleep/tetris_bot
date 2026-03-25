//! AI module wrapping Cold Clear for move computation.
//!
//! Cold Clear runs on background threads. We feed it pieces from the queue
//! and request moves, then translate its output into our input sequence.

use anyhow::{Result, Context};
use libtetris::{Board, Piece, PieceMovement};
use crate::config::AiConfig;
use crate::vision::{CellColor, GameState};

/// A computed move: the sequence of inputs to place the current piece.
#[derive(Debug, Clone)]
pub struct AiMove {
    pub inputs: Vec<MoveInput>,
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

/// Convert Cold Clear's PieceMovement sequence to our MoveInput list.
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

/// Wraps the Cold Clear AI engine.
pub struct AiEngine {
    interface: cold_clear::Interface,
    pieces_fed: usize,
}

impl AiEngine {
    pub fn new(cfg: &AiConfig) -> Result<Self> {
        let options = cold_clear::Options {
            max_nodes: 200_000,
            min_nodes: 0,
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
            _ => cold_clear::evaluation::Standard::default(), // "balanced"
        };

        let board = Board::new();
        let interface = cold_clear::Interface::launch(board, options, weights, None);

        tracing::info!("Cold Clear AI initialized (playstyle: {})", cfg.playstyle);

        Ok(Self {
            interface,
            pieces_fed: 0,
        })
    }

    /// Feed new pieces from the visible queue and request a move.
    pub fn get_move(&mut self, state: &GameState) -> Result<AiMove> {
        // Feed the current piece + queue to Cold Clear
        let mut pieces_to_add = Vec::new();
        if let Some(current) = state.current_piece {
            if let Some(p) = color_to_piece(current) {
                pieces_to_add.push(p);
            }
        }
        for color in &state.next_queue {
            if let Some(p) = color_to_piece(*color) {
                pieces_to_add.push(p);
            }
        }

        // Only feed pieces we haven't fed yet
        for piece in pieces_to_add.iter().skip(self.pieces_fed) {
            self.interface.add_next_piece(*piece);
        }
        self.pieces_fed = self.pieces_fed.max(pieces_to_add.len());

        // Request a move
        self.interface.suggest_next_move(state.incoming_garbage);

        // Block until Cold Clear provides its answer
        let (mv, _info) = self.interface.block_next_move()
            .context("Cold Clear died — no move available")?;

        // Tell Cold Clear we're playing this move
        self.interface.play_next_move(mv.expected_location);

        // Convert to our input representation
        let inputs = cc_movements_to_inputs(&mv.inputs, mv.hold);

        Ok(AiMove { inputs })
    }

}

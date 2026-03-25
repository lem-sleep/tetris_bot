mod capture;
mod vision;
mod ai;
mod input;
mod config;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use anyhow::Result;
use tracing::{info, warn};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

/// Shared state between bot thread and GUI.
pub struct BotState {
    pub enabled: AtomicBool,
    pub paused: AtomicBool,
    pub pieces_placed: AtomicU64,
    pub status_text: Mutex<String>,
    /// Rolling PPS calculated from recent piece times.
    pub current_pps: Mutex<f64>,
    /// Last few piece timestamps for PPS calculation.
    piece_times: Mutex<Vec<std::time::Instant>>,
}

impl BotState {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            pieces_placed: AtomicU64::new(0),
            status_text: Mutex::new("Stopped".into()),
            current_pps: Mutex::new(0.0),
            piece_times: Mutex::new(Vec::new()),
        }
    }

    fn record_piece(&self) {
        self.pieces_placed.fetch_add(1, Ordering::Relaxed);
        let now = std::time::Instant::now();
        let mut times = self.piece_times.lock().unwrap();
        times.push(now);
        // Keep only last 20 pieces for rolling PPS
        if times.len() > 20 {
            let excess = times.len() - 20;
            times.drain(..excess);
        }
        if times.len() >= 2 {
            let elapsed = times.last().unwrap().duration_since(*times.first().unwrap()).as_secs_f64();
            if elapsed > 0.0 {
                *self.current_pps.lock().unwrap() = (times.len() - 1) as f64 / elapsed;
            }
        }
    }

    fn set_status(&self, s: &str) {
        *self.status_text.lock().unwrap() = s.into();
    }
}

fn key_just_pressed(vk: u16) -> bool {
    let state = unsafe { GetAsyncKeyState(vk as i32) };
    (state & 1) != 0
}

fn run_bot(state: Arc<BotState>, cfg: config::BotConfig) {
    let mut capturer = match capture::ScreenCapture::new(&cfg.capture) {
        Ok(c) => c,
        Err(e) => { state.set_status(&format!("Capture init failed: {e}")); return; }
    };
    let board_reader = match vision::BoardReader::new(&cfg.vision) {
        Ok(b) => b,
        Err(e) => { state.set_status(&format!("Vision init failed: {e}")); return; }
    };
    let mut ai_engine = match ai::AiEngine::new(&cfg.ai) {
        Ok(a) => a,
        Err(e) => { state.set_status(&format!("AI init failed: {e}")); return; }
    };
    let mut input_sender = match input::InputSender::new(&cfg.input) {
        Ok(i) => i,
        Err(e) => { state.set_status(&format!("Input init failed: {e}")); return; }
    };

    let vk_toggle = cfg.hotkeys.vk_toggle;
    let vk_pause = cfg.hotkeys.vk_pause;

    info!("All subsystems initialized. Entering main loop.");
    info!("Hotkeys: F9 = Start/Stop, F10 = Pause/Resume");
    state.set_status("Stopped");

    let target_frame_time = std::time::Duration::from_millis(1000 / cfg.capture.target_fps as u64);
    let mut last_board: Option<[[vision::CellColor; 10]; 20]> = None;

    loop {
        let frame_start = std::time::Instant::now();

        // Check hotkeys
        if key_just_pressed(vk_toggle) {
            let was_enabled = state.enabled.load(Ordering::Relaxed);
            state.enabled.store(!was_enabled, Ordering::Relaxed);
            if !was_enabled {
                state.paused.store(false, Ordering::Relaxed);
                last_board = None;
                state.set_status("Running");
                info!("Bot STARTED");
            } else {
                state.set_status("Stopped");
                info!("Bot STOPPED");
            }
        }

        if key_just_pressed(vk_pause) && state.enabled.load(Ordering::Relaxed) {
            let was_paused = state.paused.load(Ordering::Relaxed);
            state.paused.store(!was_paused, Ordering::Relaxed);
            if !was_paused {
                state.set_status("Paused");
                info!("Bot PAUSED");
            } else {
                state.set_status("Running");
                info!("Bot RESUMED");
            }
        }

        if !state.enabled.load(Ordering::Relaxed) || state.paused.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }

        let frame = match capturer.grab_frame() {
            Ok(f) => f,
            Err(e) => {
                warn!("Frame capture failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(16));
                continue;
            }
        };

        let game_state = match board_reader.read_state(&frame) {
            Ok(s) => s,
            Err(e) => {
                warn!("Board read failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };

        if !game_state.is_active || game_state.current_piece.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }

        let board_changed = last_board.as_ref().map_or(true, |prev| *prev != game_state.board);

        if board_changed {
            last_board = Some(game_state.board);

            match ai_engine.get_move(&game_state) {
                Ok(ai_move) => {
                    if let Err(e) = input_sender.execute_move(&ai_move) {
                        warn!("Input execution failed: {e}");
                    } else {
                        state.record_piece();
                    }
                }
                Err(e) => { warn!("AI move failed: {e}"); }
            }
        }

        let elapsed = frame_start.elapsed();
        if elapsed < target_frame_time {
            std::thread::sleep(target_frame_time - elapsed);
        }
    }
}

struct BotApp {
    state: Arc<BotState>,
}

impl eframe::App for BotApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        // Repaint continuously so status updates appear
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        let enabled = self.state.enabled.load(Ordering::Relaxed);
        let paused = self.state.paused.load(Ordering::Relaxed);
        let pieces = self.state.pieces_placed.load(Ordering::Relaxed);
        let status = self.state.status_text.lock().unwrap().clone();
        let pps = *self.state.current_pps.lock().unwrap();

        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("TETR.IO Bot");
            ui.separator();

            // Status with color
            ui.horizontal(|ui| {
                ui.label("Status:");
                let (color, text) = if !enabled {
                    (eframe::egui::Color32::from_rgb(180, 60, 60), &status)
                } else if paused {
                    (eframe::egui::Color32::from_rgb(200, 180, 50), &status)
                } else {
                    (eframe::egui::Color32::from_rgb(60, 180, 60), &status)
                };
                ui.colored_label(color, eframe::egui::RichText::new(text).size(18.0).strong());
            });

            ui.add_space(8.0);

            // Stats
            eframe::egui::Grid::new("stats").num_columns(2).spacing([20.0, 6.0]).show(ui, |ui| {
                ui.label("Pieces placed:");
                ui.label(eframe::egui::RichText::new(format!("{pieces}")).strong());
                ui.end_row();

                ui.label("Current PPS:");
                ui.label(eframe::egui::RichText::new(format!("{pps:.1}")).strong());
                ui.end_row();
            });

            ui.add_space(12.0);

            // Controls
            ui.horizontal(|ui| {
                if ui.button(if enabled { "Stop (F9)" } else { "Start (F9)" }).clicked() {
                    let was = self.state.enabled.load(Ordering::Relaxed);
                    self.state.enabled.store(!was, Ordering::Relaxed);
                    if was {
                        self.state.paused.store(false, Ordering::Relaxed);
                        self.state.set_status("Stopped");
                    } else {
                        self.state.set_status("Running");
                    }
                }

                if enabled {
                    if ui.button(if paused { "Resume (F10)" } else { "Pause (F10)" }).clicked() {
                        let was = self.state.paused.load(Ordering::Relaxed);
                        self.state.paused.store(!was, Ordering::Relaxed);
                        if was {
                            self.state.set_status("Running");
                        } else {
                            self.state.set_status("Paused");
                        }
                    }
                }
            });

            ui.add_space(12.0);
            ui.separator();

            // Hotkey reference
            ui.label(eframe::egui::RichText::new("Hotkeys").size(14.0).strong());
            eframe::egui::Grid::new("hotkeys").num_columns(2).spacing([20.0, 4.0]).show(ui, |ui| {
                ui.label("F9");
                ui.label("Start / Stop");
                ui.end_row();
                ui.label("F10");
                ui.label("Pause / Resume");
                ui.end_row();
            });
        });
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tetris_bot=info".parse()?),
        )
        .init();

    info!("TETR.IO Bot starting up...");

    let cfg = config::BotConfig::load("config.toml")?;
    info!(
        "Config loaded: capture region {}x{} at ({}, {})",
        cfg.capture.width, cfg.capture.height, cfg.capture.x, cfg.capture.y
    );

    let state = Arc::new(BotState::new());
    let state_clone = Arc::clone(&state);

    // Spawn bot thread
    std::thread::spawn(move || {
        run_bot(state_clone, cfg);
    });

    // Run GUI on main thread
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([320.0, 300.0])
            .with_always_on_top(),
        ..Default::default()
    };

    eframe::run_native(
        "TETR.IO Bot",
        options,
        Box::new(move |_cc| Ok(Box::new(BotApp { state }))),
    ).map_err(|e| anyhow::anyhow!("GUI error: {e}"))?;

    Ok(())
}

mod capture;
mod vision;
mod ai;
mod config;
mod overlay;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use anyhow::Result;
use tracing::{info, warn, debug};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

/// Shared state between bot thread and GUI.
pub struct BotState {
    pub enabled: AtomicBool,
    pub paused: AtomicBool,
    pub status_text: Mutex<String>,
    /// Current AI suggestion (direct + hold placements) for the overlay.
    pub current_suggestion: Mutex<Option<ai::AiSuggestion>>,
}

impl BotState {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            status_text: Mutex::new("Stopped".into()),
            current_suggestion: Mutex::new(None),
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
    let mut board_reader = match vision::BoardReader::new(&cfg.vision) {
        Ok(b) => b,
        Err(e) => { state.set_status(&format!("Vision init failed: {e}")); return; }
    };
    let mut ai_engine = match ai::AiEngine::new(&cfg.ai) {
        Ok(a) => a,
        Err(e) => { state.set_status(&format!("AI init failed: {e}")); return; }
    };

    let vk_toggle = cfg.hotkeys.vk_toggle;
    let vk_pause = cfg.hotkeys.vk_pause;

    info!("All subsystems initialized. Entering main loop.");
    info!("Hotkeys: F9 = Start/Stop, F10 = Pause/Resume");
    state.set_status("Stopped");

    let target_frame_time = std::time::Duration::from_millis(1000 / cfg.capture.target_fps as u64);
    let mut last_board: Option<[[vision::CellColor; 10]; 20]> = None;
    let mut waiting_for_move = false;
    let mut waiting_since: Option<std::time::Instant> = None;

    loop {
        let frame_start = std::time::Instant::now();

        // Check hotkeys
        if key_just_pressed(vk_toggle) {
            let was_enabled = state.enabled.load(Ordering::Relaxed);
            state.enabled.store(!was_enabled, Ordering::Relaxed);
            if !was_enabled {
                state.paused.store(false, Ordering::Relaxed);
                last_board = None;
                waiting_for_move = false;
                *state.current_suggestion.lock().unwrap() = None;
                // Reset AI engine so stale blocked workers don't linger
                if let Err(e) = ai_engine.reset() {
                    warn!("AI engine reset failed: {e}");
                }
                state.set_status("Running");
                info!("ESP STARTED");
            } else {
                *state.current_suggestion.lock().unwrap() = None;
                state.set_status("Stopped");
                info!("ESP STOPPED");
            }
        }

        if key_just_pressed(vk_pause) && state.enabled.load(Ordering::Relaxed) {
            let was_paused = state.paused.load(Ordering::Relaxed);
            state.paused.store(!was_paused, Ordering::Relaxed);
            if !was_paused {
                state.set_status("Paused");
                info!("ESP PAUSED");
            } else {
                state.set_status("Running");
                info!("ESP RESUMED");
            }
        }

        if !state.enabled.load(Ordering::Relaxed) || state.paused.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }

        // === Poll for completed AI suggestion (non-blocking) ===
        if waiting_for_move {
            if let Some(suggestion) = ai_engine.poll_move() {
                waiting_for_move = false;
                waiting_since = None;
                debug!("AI suggestion ready: direct={:?}", suggestion.direct.cells);
                *state.current_suggestion.lock().unwrap() = Some(suggestion);
            } else if let Some(since) = waiting_since {
                // Timeout: if AI hasn't responded in 5 seconds, give up and allow resubmission
                if since.elapsed().as_secs_f64() > 5.0 {
                    warn!("AI move timeout — clearing wait flag");
                    waiting_for_move = false;
                    waiting_since = None;
                }
            }
        }

        // === Capture frame ===
        let frame = match capturer.grab_frame() {
            Ok(f) => f,
            Err(e) => {
                warn!("Frame capture failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(16));
                continue;
            }
        };

        // Skip heavy processing if DXGI returned a cached frame AND
        // our sentinel pixel check says nothing changed.
        if !capturer.frame_is_new && !board_reader.is_frame_dirty(&frame) {
            let elapsed = frame_start.elapsed();
            if elapsed < target_frame_time {
                std::thread::sleep(target_frame_time - elapsed);
            }
            continue;
        }

        // === Full vision pass ===
        let vision_start = std::time::Instant::now();
        let game_state = match board_reader.read_state(&frame) {
            Ok(s) => s,
            Err(e) => {
                warn!("Board read failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };
        let vision_ms = vision_start.elapsed().as_secs_f64() * 1000.0;
        debug!("Vision: {vision_ms:.2}ms");

        if !game_state.is_active || game_state.current_piece.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }

        // === Submit to AI if board changed (non-blocking) ===
        let board_changed = last_board.as_ref().map_or(true, |prev| *prev != game_state.board);

        if board_changed && !waiting_for_move {
            last_board = Some(game_state.board);
            // Keep old suggestion visible until new one arrives (no clearing)
            ai_engine.submit(&game_state);
            waiting_for_move = true;
            waiting_since = Some(std::time::Instant::now());
            debug!("Submitted new state to AI");
        }

        let elapsed = frame_start.elapsed();
        if elapsed < target_frame_time {
            std::thread::sleep(target_frame_time - elapsed);
        }
    }
}

fn cell_color_name(c: vision::CellColor) -> &'static str {
    match c {
        vision::CellColor::I => "I",
        vision::CellColor::O => "O",
        vision::CellColor::T => "T",
        vision::CellColor::S => "S",
        vision::CellColor::Z => "Z",
        vision::CellColor::J => "J",
        vision::CellColor::L => "L",
        _ => "?",
    }
}

/// Map a CellColor to an egui RGBA color for the control panel display.
fn piece_panel_color(piece: vision::CellColor) -> eframe::egui::Color32 {
    use eframe::egui::Color32;
    match piece {
        vision::CellColor::I => Color32::from_rgb(0, 220, 220),
        vision::CellColor::O => Color32::from_rgb(220, 220, 0),
        vision::CellColor::T => Color32::from_rgb(160, 0, 220),
        vision::CellColor::S => Color32::from_rgb(0, 220, 0),
        vision::CellColor::Z => Color32::from_rgb(220, 0, 0),
        vision::CellColor::J => Color32::from_rgb(0, 0, 220),
        vision::CellColor::L => Color32::from_rgb(220, 130, 0),
        _ => Color32::from_rgb(180, 180, 180),
    }
}

struct BotApp {
    state: Arc<BotState>,
}

impl eframe::App for BotApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        let enabled = self.state.enabled.load(Ordering::Relaxed);
        let paused = self.state.paused.load(Ordering::Relaxed);
        let status = self.state.status_text.lock().unwrap().clone();
        let suggestion = self.state.current_suggestion.lock().unwrap().clone();

        // === Control panel ===
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("TETR.IO ESP");
            ui.separator();

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

            // Show current recommendation info
            if let Some(ref sg) = suggestion {
                ui.horizontal(|ui| {
                    ui.label("Direct:");
                    let color = piece_panel_color(sg.direct.piece_color);
                    let name = cell_color_name(sg.direct.piece_color);
                    ui.colored_label(color, eframe::egui::RichText::new(name).size(18.0).strong());
                });
                if let Some(ref hold) = sg.hold_option {
                    ui.horizontal(|ui| {
                        ui.label("Hold:");
                        let color = piece_panel_color(hold.piece_color);
                        let name = cell_color_name(hold.piece_color);
                        ui.colored_label(color, eframe::egui::RichText::new(name).size(18.0).strong());
                    });
                }
            } else if enabled && !paused {
                ui.label("Thinking...");
            }

            ui.add_space(12.0);

            ui.horizontal(|ui| {
                if ui.button(if enabled { "Stop (F9)" } else { "Start (F9)" }).clicked() {
                    let was = self.state.enabled.load(Ordering::Relaxed);
                    self.state.enabled.store(!was, Ordering::Relaxed);
                    if was {
                        self.state.paused.store(false, Ordering::Relaxed);
                        *self.state.current_suggestion.lock().unwrap() = None;
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
    // Log to both stderr and a file so we can review after the fact
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let log_file = std::fs::File::create("bot.log")
        .expect("Failed to create bot.log");

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr);

    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("tetris_bot=debug".parse()?);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    info!("TETR.IO ESP starting up...");

    let cfg = config::BotConfig::load("config.toml")?;
    info!(
        "Config loaded: capture region {}x{} at ({}, {})",
        cfg.capture.width, cfg.capture.height, cfg.capture.x, cfg.capture.y
    );

    // Extract overlay rendering params before moving config into bot thread
    let board_screen_x = (cfg.capture.x + cfg.vision.board_x) as f32;
    let board_screen_y = (cfg.capture.y + cfg.vision.board_y) as f32;
    let cell_size = cfg.vision.cell_size as f32;
    let ghost_opacity = cfg.overlay.ghost_opacity;

    let state = Arc::new(BotState::new());

    // Spawn bot thread (capture + AI).
    let state_bot = Arc::clone(&state);
    std::thread::spawn(move || {
        run_bot(state_bot, cfg);
    });

    // Spawn native Win32 overlay thread.
    let state_overlay = Arc::clone(&state);
    std::thread::spawn(move || {
        overlay::run_overlay(state_overlay, board_screen_x, board_screen_y, cell_size, ghost_opacity);
    });

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([320.0, 260.0])
            .with_always_on_top(),
        ..Default::default()
    };

    eframe::run_native(
        "TETR.IO ESP",
        options,
        Box::new(move |_cc| Ok(Box::new(BotApp { state }))),
    ).map_err(|e| anyhow::anyhow!("GUI error: {e}"))?;

    Ok(())
}

//! Input simulation module with humanization.
//!
//! Sends keyboard inputs via Windows SendInput API with realistic
//! timing jitter to mimic human play patterns. Features dynamic PPS
//! that adjusts based on board complexity.

use anyhow::Result;
use rand::Rng;
use rand_distr::{Distribution, Normal};
use std::mem;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use crate::ai::{AiMove, MoveInput};
use crate::config::InputConfig;

pub struct InputSender {
    base_delay_ms: f64,
    jitter_std_ms: f64,
    key_hold_ms: f64,
    key_hold_jitter_ms: f64,
    arr_ms: f64,
    think_pause_chance: f64,
    think_pause_max_ms: f64,
    pps_target_min: f64,
    pps_target_max: f64,

    vk_left: u16,
    vk_right: u16,
    vk_rotate_cw: u16,
    vk_rotate_ccw: u16,
    vk_rotate_180: u16,
    vk_soft_drop: u16,
    vk_hard_drop: u16,
    vk_hold: u16,

    last_piece_time: std::time::Instant,
    /// Precomputed scan codes to avoid calling MapVirtualKeyW per keypress.
    scan_cache: std::collections::HashMap<u16, u16>,
}

impl InputSender {
    pub fn new(cfg: &InputConfig) -> Result<Self> {
        let mut scan_cache = std::collections::HashMap::new();
        // Precompute scan codes for all keybinds
        for &vk in &[
            cfg.vk_left, cfg.vk_right, cfg.vk_rotate_cw, cfg.vk_rotate_ccw,
            cfg.vk_rotate_180, cfg.vk_soft_drop, cfg.vk_hard_drop, cfg.vk_hold,
        ] {
            let scan = unsafe { MapVirtualKeyW(vk as u32, MAP_VIRTUAL_KEY_TYPE(0)) } as u16;
            scan_cache.insert(vk, scan);
        }

        Ok(Self {
            base_delay_ms: cfg.base_delay_ms,
            jitter_std_ms: cfg.jitter_std_ms,
            key_hold_ms: cfg.key_hold_ms,
            key_hold_jitter_ms: cfg.key_hold_jitter_ms,
            arr_ms: cfg.arr_ms,
            think_pause_chance: cfg.think_pause_chance,
            think_pause_max_ms: cfg.think_pause_max_ms,
            pps_target_min: cfg.pps_target_min,
            pps_target_max: cfg.pps_target_max,
            vk_left: cfg.vk_left,
            vk_right: cfg.vk_right,
            vk_rotate_cw: cfg.vk_rotate_cw,
            vk_rotate_ccw: cfg.vk_rotate_ccw,
            vk_rotate_180: cfg.vk_rotate_180,
            vk_soft_drop: cfg.vk_soft_drop,
            vk_hard_drop: cfg.vk_hard_drop,
            vk_hold: cfg.vk_hold,
            last_piece_time: std::time::Instant::now(),
            scan_cache,
        })
    }

    /// Execute a full AI move with humanized timing.
    /// `board_height` (0-20) controls dynamic PPS: low board = faster play.
    pub fn execute_move(&mut self, ai_move: &AiMove) -> Result<()> {
        let mut rng = rand::thread_rng();

        // Dynamic PPS based on board height
        let height = ai_move.board_height as f64;
        // Low board (0-6): play at max PPS. High board (15+): slow down.
        let pressure = (height / 20.0).clamp(0.0, 1.0);
        let pps_min = self.pps_target_min + (1.0 - pressure) * 1.5; // boost when safe
        let pps_max = self.pps_target_max + (1.0 - pressure) * 2.0;
        self.enforce_pps_limit_dynamic(pps_min, pps_max, &mut rng);

        // Thinking pause — less likely when board is low (playing fast)
        let think_chance = self.think_pause_chance * (0.3 + 0.7 * pressure);
        if rng.gen_bool(think_chance.clamp(0.0, 1.0)) {
            let max_pause = self.think_pause_max_ms * (0.4 + 0.6 * pressure);
            let pause = rng.gen_range(10.0..max_pause.max(15.0));
            self.sleep_ms(pause);
        }

        for (i, input) in ai_move.inputs.iter().enumerate() {
            let vk = self.input_to_vk(*input);

            let is_lateral = matches!(input, MoveInput::Left | MoveInput::Right);
            let prev_same = i > 0 && ai_move.inputs[i - 1] == *input && is_lateral;

            if prev_same {
                let arr_delay = self.jittered_duration(self.arr_ms.max(3.0), 1.5, &mut rng);
                self.sleep_ms(arr_delay);
            } else if is_lateral && i > 0 && matches!(ai_move.inputs[i - 1], MoveInput::Left | MoveInput::Right) {
                let switch_delay = self.jittered_duration(self.base_delay_ms * 1.2, self.jitter_std_ms, &mut rng);
                self.sleep_ms(switch_delay);
            }

            self.send_key_down(vk)?;
            let hold = self.jittered_duration(self.key_hold_ms, self.key_hold_jitter_ms, &mut rng);
            self.sleep_ms(hold);
            self.send_key_up(vk)?;

            if i + 1 < ai_move.inputs.len() {
                // Slightly faster inter-key when board is low
                let speed_factor = 1.0 - (1.0 - pressure) * 0.25;
                let delay = self.jittered_duration(self.base_delay_ms * speed_factor, self.jitter_std_ms, &mut rng);
                self.sleep_ms(delay);
            }
        }

        self.last_piece_time = std::time::Instant::now();
        Ok(())
    }

    fn enforce_pps_limit_dynamic(&self, pps_min: f64, pps_max: f64, rng: &mut impl Rng) {
        let target_pps = rng.gen_range(pps_min..pps_max.max(pps_min + 0.1));
        let min_interval_ms = 1000.0 / target_pps;
        let elapsed_ms = self.last_piece_time.elapsed().as_secs_f64() * 1000.0;
        if elapsed_ms < min_interval_ms {
            self.sleep_ms(min_interval_ms - elapsed_ms);
        }
    }

    fn input_to_vk(&self, input: MoveInput) -> u16 {
        match input {
            MoveInput::Left => self.vk_left,
            MoveInput::Right => self.vk_right,
            MoveInput::RotateCW => self.vk_rotate_cw,
            MoveInput::RotateCCW => self.vk_rotate_ccw,
            MoveInput::Rotate180 => self.vk_rotate_180,
            MoveInput::SoftDrop => self.vk_soft_drop,
            MoveInput::HardDrop => self.vk_hard_drop,
            MoveInput::Hold => self.vk_hold,
        }
    }

    fn jittered_duration(&self, base: f64, std_dev: f64, rng: &mut impl Rng) -> f64 {
        if std_dev <= 0.0 {
            return base.max(1.0);
        }
        let normal = Normal::new(base, std_dev).unwrap();
        normal.sample(rng).max(1.0)
    }

    fn sleep_ms(&self, ms: f64) {
        std::thread::sleep(std::time::Duration::from_micros((ms * 1000.0) as u64));
    }

    fn send_key_down(&self, vk: u16) -> Result<()> {
        let scan = self.scan_cache.get(&vk).copied().unwrap_or(0);
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], mem::size_of::<INPUT>() as i32) };
        if sent != 1 {
            anyhow::bail!("SendInput key_down failed for VK 0x{vk:04X}");
        }
        Ok(())
    }

    fn send_key_up(&self, vk: u16) -> Result<()> {
        let scan = self.scan_cache.get(&vk).copied().unwrap_or(0);
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], mem::size_of::<INPUT>() as i32) };
        if sent != 1 {
            anyhow::bail!("SendInput key_up failed for VK 0x{vk:04X}");
        }
        Ok(())
    }
}

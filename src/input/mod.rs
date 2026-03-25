//! Input simulation module with humanization.
//!
//! Sends keyboard inputs via Windows SendInput API with realistic
//! timing jitter to mimic human play patterns.

use anyhow::Result;
use rand::Rng;
use rand_distr::{Distribution, Normal};
use std::mem;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use crate::ai::{AiMove, MoveInput};
use crate::config::InputConfig;

/// Handles sending keyboard inputs to TETR.IO with humanized timing.
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

    /// Tracks time of last hard drop to enforce PPS limits.
    last_piece_time: std::time::Instant,
}

impl InputSender {
    pub fn new(cfg: &InputConfig) -> Result<Self> {
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
        })
    }

    /// Execute a full AI move as a sequence of humanized key presses.
    pub fn execute_move(&mut self, ai_move: &AiMove) -> Result<()> {
        let mut rng = rand::thread_rng();

        // Enforce PPS limit: wait if we're placing pieces too fast
        self.enforce_pps_limit(&mut rng);

        // Optional "thinking" pause — simulates a human studying the board
        if rng.gen_bool(self.think_pause_chance.clamp(0.0, 1.0)) {
            let pause = rng.gen_range(20.0..self.think_pause_max_ms);
            self.sleep_ms(pause);
        }

        for (i, input) in ai_move.inputs.iter().enumerate() {
            let vk = self.input_to_vk(*input);

            // Simulate DAS behavior for repeated lateral moves
            let is_lateral = matches!(input, MoveInput::Left | MoveInput::Right);
            let prev_same = i > 0 && ai_move.inputs[i - 1] == *input && is_lateral;

            if prev_same {
                // ARR timing for repeated same-direction taps
                let arr_delay = self.jittered_duration(self.arr_ms.max(5.0), 2.0, &mut rng);
                self.sleep_ms(arr_delay);
            } else if is_lateral && i > 0 && matches!(ai_move.inputs[i - 1], MoveInput::Left | MoveInput::Right) {
                // Switching direction — add a small extra hesitation
                let switch_delay = self.jittered_duration(self.base_delay_ms * 1.3, self.jitter_std_ms, &mut rng);
                self.sleep_ms(switch_delay);
            }

            // Press key
            self.send_key_down(vk)?;
            let hold = self.jittered_duration(self.key_hold_ms, self.key_hold_jitter_ms, &mut rng);
            self.sleep_ms(hold);
            self.send_key_up(vk)?;

            // Inter-key delay (skip after last input)
            if i + 1 < ai_move.inputs.len() {
                let delay = self.jittered_duration(self.base_delay_ms, self.jitter_std_ms, &mut rng);
                self.sleep_ms(delay);
            }
        }

        self.last_piece_time = std::time::Instant::now();
        Ok(())
    }

    /// Wait if needed to keep PPS within the target human-like range.
    fn enforce_pps_limit(&self, rng: &mut impl Rng) {
        let target_pps = rng.gen_range(self.pps_target_min..self.pps_target_max);
        let min_interval_ms = 1000.0 / target_pps;
        let elapsed_ms = self.last_piece_time.elapsed().as_secs_f64() * 1000.0;

        if elapsed_ms < min_interval_ms {
            let wait = min_interval_ms - elapsed_ms;
            self.sleep_ms(wait);
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

    /// Send a key-down event via Windows SendInput.
    fn send_key_down(&self, vk: u16) -> Result<()> {
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAP_VIRTUAL_KEY_TYPE(0)) } as u16;
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

    /// Send a key-up event via Windows SendInput.
    fn send_key_up(&self, vk: u16) -> Result<()> {
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAP_VIRTUAL_KEY_TYPE(0)) } as u16;
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

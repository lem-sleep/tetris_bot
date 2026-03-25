//! Configuration module — loads settings from a TOML file.

use anyhow::{Result, Context};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct BotConfig {
    pub capture: CaptureConfig,
    pub vision: VisionConfig,
    pub ai: AiConfig,
    pub input: InputConfig,
    #[serde(default)]
    pub hotkeys: HotkeyConfig,
}

#[derive(Debug, Deserialize)]
pub struct CaptureConfig {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_fps")]
    pub target_fps: u32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct VisionConfig {
    pub board_x: u32,
    pub board_y: u32,
    pub cell_size: u32,
    pub hold_x: u32,
    pub hold_y: u32,
    pub next_positions: Vec<(u32, u32)>,
    #[serde(default = "default_tolerance")]
    pub color_tolerance: u8,
}

#[derive(Debug, Deserialize)]
pub struct AiConfig {
    #[serde(default = "default_playstyle")]
    pub playstyle: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct InputConfig {
    #[serde(default = "default_base_delay")]
    pub base_delay_ms: f64,
    #[serde(default = "default_jitter")]
    pub jitter_std_ms: f64,
    #[serde(default = "default_key_hold")]
    pub key_hold_ms: f64,
    #[serde(default = "default_key_hold_jitter")]
    pub key_hold_jitter_ms: f64,
    #[serde(default = "default_das")]
    pub das_ms: f64,
    #[serde(default = "default_arr")]
    pub arr_ms: f64,
    #[serde(default = "default_think_chance")]
    pub think_pause_chance: f64,
    #[serde(default = "default_think_max")]
    pub think_pause_max_ms: f64,
    #[serde(default = "default_pps_min")]
    pub pps_target_min: f64,
    #[serde(default = "default_pps_max")]
    pub pps_target_max: f64,

    #[serde(default = "vk_left")]
    pub vk_left: u16,
    #[serde(default = "vk_right")]
    pub vk_right: u16,
    #[serde(default = "vk_rotate_cw")]
    pub vk_rotate_cw: u16,
    #[serde(default = "vk_rotate_ccw")]
    pub vk_rotate_ccw: u16,
    #[serde(default = "vk_rotate_180")]
    pub vk_rotate_180: u16,
    #[serde(default = "vk_soft_drop")]
    pub vk_soft_drop: u16,
    #[serde(default = "vk_hard_drop")]
    pub vk_hard_drop: u16,
    #[serde(default = "vk_hold")]
    pub vk_hold: u16,
}

#[derive(Debug, Deserialize)]
pub struct HotkeyConfig {
    #[serde(default = "vk_toggle")]
    pub vk_toggle: u16,
    #[serde(default = "vk_pause")]
    pub vk_pause: u16,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            vk_toggle: vk_toggle(),
            vk_pause: vk_pause(),
        }
    }
}

impl BotConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .context(format!("Failed to read config file: {path}"))?;
        toml::from_str(&content).context("Failed to parse config TOML")
    }
}

fn default_fps() -> u32 { 60 }
fn default_tolerance() -> u8 { 40 }
fn default_playstyle() -> String { "balanced".into() }
fn default_base_delay() -> f64 { 28.0 }
fn default_jitter() -> f64 { 8.0 }
fn default_key_hold() -> f64 { 18.0 }
fn default_key_hold_jitter() -> f64 { 5.0 }
fn default_das() -> f64 { 100.0 }
fn default_arr() -> f64 { 0.0 }
fn default_think_chance() -> f64 { 0.12 }
fn default_think_max() -> f64 { 180.0 }
fn default_pps_min() -> f64 { 4.0 }
fn default_pps_max() -> f64 { 6.5 }

// Default TETR.IO keybinds
fn vk_left() -> u16 { 0x25 }       // VK_LEFT
fn vk_right() -> u16 { 0x27 }      // VK_RIGHT
fn vk_rotate_cw() -> u16 { 0x26 }  // VK_UP
fn vk_rotate_ccw() -> u16 { 0x5A } // Z
fn vk_rotate_180() -> u16 { 0x41 } // A
fn vk_soft_drop() -> u16 { 0x28 }  // VK_DOWN
fn vk_hard_drop() -> u16 { 0x20 }  // Space
fn vk_hold() -> u16 { 0x43 }       // C

// Hotkey defaults
fn vk_toggle() -> u16 { 0x78 }     // F9
fn vk_pause() -> u16 { 0x79 }      // F10

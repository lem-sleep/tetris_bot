//! Configuration module — loads settings from a TOML file.

use anyhow::{Result, Context};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct BotConfig {
    pub capture: CaptureConfig,
    pub vision: VisionConfig,
    pub ai: AiConfig,
    #[serde(default)]
    pub overlay: OverlayConfig,
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
    #[serde(default = "default_max_nodes")]
    pub max_nodes: u32,
    #[serde(default = "default_min_nodes")]
    pub min_nodes: u32,
    #[serde(default = "default_movement_mode")]
    pub movement_mode: String,
}

#[derive(Debug, Deserialize)]
pub struct OverlayConfig {
    #[serde(default = "default_ghost_opacity")]
    pub ghost_opacity: u8,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            ghost_opacity: default_ghost_opacity(),
        }
    }
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
fn default_max_nodes() -> u32 { 400_000 }
fn default_min_nodes() -> u32 { 0 }
fn default_movement_mode() -> String { "hard_drop_only".into() }
fn default_ghost_opacity() -> u8 { 120 }

// Hotkey defaults
fn vk_toggle() -> u16 { 0x78 }     // F9
fn vk_pause() -> u16 { 0x79 }      // F10

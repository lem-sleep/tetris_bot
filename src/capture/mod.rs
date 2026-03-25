//! Screen capture module using DXGI Desktop Duplication API via dxgi-capture-rs.

use anyhow::{Result, Context};
use dxgi_capture_rs::DXGIManager;
use crate::config::CaptureConfig;
use std::sync::Arc;

/// Raw captured frame data (BGRA pixel buffer).
/// Uses Arc<Vec<u8>> for cheap cloning — the pixel data is shared, not copied.
#[derive(Clone)]
pub struct Frame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Frame {
    /// Get pixel at (x, y) as (R, G, B).
    #[inline(always)]
    pub fn pixel_rgb(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let offset = (y * self.stride + x * 4) as usize;
        unsafe {
            let b = *self.data.get_unchecked(offset);
            let g = *self.data.get_unchecked(offset + 1);
            let r = *self.data.get_unchecked(offset + 2);
            (r, g, b)
        }
    }

    /// Extract a sub-region of the frame.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Frame {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in y..(y + h) {
            let src_start = (row * self.stride + x * 4) as usize;
            let src_end = src_start + (w * 4) as usize;
            data.extend_from_slice(&self.data[src_start..src_end]);
        }
        Frame {
            data: Arc::new(data),
            width: w,
            height: h,
            stride: w * 4,
        }
    }
}

/// Screen capturer wrapping dxgi-capture-rs.
pub struct ScreenCapture {
    manager: DXGIManager,
    region_x: u32,
    region_y: u32,
    region_w: u32,
    region_h: u32,
    last_frame: Option<Frame>,
    /// True if the last grab returned a fresh frame from DXGI (vs cached).
    pub frame_is_new: bool,
}

impl ScreenCapture {
    pub fn new(cfg: &CaptureConfig) -> Result<Self> {
        let manager = DXGIManager::new(16)
            .context("Failed to initialize DXGI Desktop Duplication")?;

        let (screen_w, screen_h) = manager.geometry();
        tracing::info!("DXGI initialized: screen {screen_w}x{screen_h}");

        Ok(Self {
            manager,
            region_x: cfg.x,
            region_y: cfg.y,
            region_w: cfg.width,
            region_h: cfg.height,
            last_frame: None,
            frame_is_new: false,
        })
    }

    /// Grab a single frame, cropped to the region of interest.
    /// On DXGI timeout (no screen change), returns the last cached frame.
    pub fn grab_frame(&mut self) -> Result<Frame> {
        match self.manager.capture_frame_components() {
            Ok((pixels, (w, h))) => {
                let stride = w as u32 * 4;
                let full_frame = Frame {
                    data: Arc::new(pixels),
                    width: w as u32,
                    height: h as u32,
                    stride,
                };

                let frame = if self.region_x == 0 && self.region_y == 0
                    && self.region_w == full_frame.width
                    && self.region_h == full_frame.height
                {
                    full_frame
                } else {
                    full_frame.crop(
                        self.region_x,
                        self.region_y,
                        self.region_w.min(full_frame.width - self.region_x),
                        self.region_h.min(full_frame.height - self.region_y),
                    )
                };

                self.last_frame = Some(frame.clone());
                self.frame_is_new = true;
                Ok(frame)
            }
            Err(_) => {
                self.frame_is_new = false;
                self.last_frame.clone()
                    .ok_or_else(|| anyhow::anyhow!("No frame captured yet (DXGI timeout on first capture)"))
            }
        }
    }
}

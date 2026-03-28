# TETR.IO ESP Helper

A real-time AI placement assistant for TETR.IO, written in Rust. It reads the game state via screen capture, uses Cold Clear to compute the optimal piece placement, and draws a transparent ghost overlay directly on the playfield showing exactly where to place your current piece (and hold piece). **No inputs are sent — you play manually.**

## What It Does

- Captures the TETR.IO screen using the DXGI Desktop Duplication API
- Reads the board, current piece, hold piece, and next queue via computer vision
- Computes the best placement for the current piece and the best placement if you hold first
- Draws both options as colored ghost pieces directly over the game using a transparent Win32 layered window
- Pre-computes placements for upcoming queue pieces in the background so the ghost appears instantly when each new piece spawns

## Architecture

```
┌─────────────────┐    DXGI     ┌───────────────┐   pixels  ┌──────────────┐
│  TETR.IO Client  │────────────▶│ Screen Capture │──────────▶│ Board Reader │
│  (unmodified)    │             │  (capture/)    │           │  (vision/)   │
└─────────────────┘             └───────────────┘           └──────┬───────┘
                                                                    │ GameState
                                                                    ▼
┌─────────────────┐  suggestion  ┌───────────────┐  Pieces  ┌──────────────┐
│ Win32 Overlay    │◀────────────│   AI Engine    │◀─────────│  Cold Clear  │
│  (overlay.rs)    │  ghost cells │   (ai/)        │  moves   │  (external)  │
└─────────────────┘             └───────────────┘           └──────────────┘
         ▲
         │ shared state (Arc<BotState>)
┌────────┴────────┐
│   egui Control  │
│     Panel       │
└─────────────────┘
```

## How It Works

### 1. Screen Capture

Uses the DXGI Desktop Duplication API (`dxgi-capture-rs`) for zero-copy GPU-accelerated frame capture. A sentinel pixel check skips full processing on unchanged frames, keeping CPU usage minimal.

### 2. Computer Vision

`BoardReader` samples pixel centers of each cell in the 10×20 grid, the hold box, and the 5-slot next queue. A 64×64×64 RGB lookup table (LUT) classifies colors in O(1) — piece colors are identified by HSV hue ranges calibrated from actual TETR.IO screenshots.

A connected-component flood fill strips the falling piece from the board before feeding it to the AI, so Cold Clear always sees a clean locked board.

### 3. AI Engine (Cold Clear)

Cold Clear runs on a background worker thread. For each new piece spawn:

1. **Primary computation** — direct placement for the current piece + placement if you hold first. Result is sent immediately when ready.
2. **Prefetch pipeline** — while the current piece is falling, the worker pre-computes placements for the next 3 queue pieces in the background. When those pieces spawn, their ghost appears **instantly** with no thinking delay.

If a new primary request arrives mid-prefetch, prefetching is interrupted and the new request is handled immediately.

### 4. Transparent Overlay

A native Win32 layered window (`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST`) covers the board with click-through transparency. Uses `UpdateLayeredWindow` with `ULW_ALPHA` and per-pixel pre-multiplied alpha — the same technique used by Discord, Steam, and other game overlays.

Two placements are drawn simultaneously:
- **Bright ghost** — where to place the current piece directly
- **Dimmer ghost** — where the hold piece would go if you swap first

### 5. Ghost Stability

The AI is queried **exactly once per piece spawn**, not on every frame. The recommendation locks in the moment a new piece is detected and stays fixed until the piece is placed, regardless of how the falling piece moves. No flashing, no jitter.

## File Structure

| File | Purpose |
|------|---------|
| `src/main.rs` | Entry point. Bot loop (capture → vision → AI → overlay), hotkey handling, egui control panel, prefetch cache management. |
| `src/capture/mod.rs` | DXGI screen capture wrapper. Caches last frame; includes dirty-pixel check to skip unchanged frames. |
| `src/vision/mod.rs` | Board reader. RGB→CellColor LUT, connected-component falling-piece detection, hold/next queue sampling. |
| `src/ai/mod.rs` | Cold Clear wrapper. Per-piece-spawn computation, two-channel worker (primary + prefetch), cancellable polling, `catch_unwind` for stability. |
| `src/overlay.rs` | Win32 layered-window overlay. 32-bit DIB section, per-pixel alpha, 60fps render loop drawing direct and hold ghost cells. |
| `src/config/mod.rs` | Config loader. Reads `config.toml` via serde into typed structs. |
| `config.toml` | All tunable parameters — capture region, board pixel coordinates, AI playstyle, ghost opacity, hotkeys. |

## Controls

| Key | Action |
|-----|--------|
| F9  | Start / Stop |
| F10 | Pause / Resume |

The egui control panel (always-on-top) shows status and the current piece recommendation. The overlay draws the ghost directly on the game.

## Setup

1. Install Rust: https://rustup.rs
2. Clone and build:
   ```
   git clone https://github.com/lem-sleep/tetris_bot.git
   cd tetris_bot
   cargo build --release
   ```
3. Open TETR.IO Desktop in **1920×1080 borderless windowed** with the **default skin** and **minimal graphics**
4. Run:
   ```
   target\release\tetris_bot.exe
   ```
5. Start a Zen mode game and press **F9** to activate

## Configuration (`config.toml`)

```toml
[capture]
x = 0              # Screen region to capture (top-left corner)
y = 0
width = 1920
height = 1080
target_fps = 60

[vision]
board_x = 733      # Pixel coordinate of top-left board cell
board_y = 86
cell_size = 45     # Cell size in pixels
hold_x = 618       # Hold piece sample point
hold_y = 190
next_positions = [ # 5 next-queue sample points
    [1310, 218],
    [1310, 354],
    [1310, 467],
    [1310, 603],
    [1310, 717],
]

[ai]
playstyle = "balanced"    # balanced | aggressive | defensive | tspin
max_nodes = 400000        # Higher = stronger but slower AI
movement_mode = "hard_drop_only"

[overlay]
ghost_opacity = 120       # 0–255 ghost piece transparency

[hotkeys]
vk_toggle = 0x78          # F9
vk_pause  = 0x79          # F10
```

If your screen layout differs (different resolution, window position, or skin), update `board_x`, `board_y`, `cell_size`, `hold_x`/`hold_y`, and `next_positions` to match your setup.

## Dependencies

- **[dxgi-capture-rs](https://crates.io/crates/dxgi-capture-rs)** — DXGI Desktop Duplication for zero-copy screen capture
- **[cold-clear](https://github.com/MinusKelvin/cold-clear)** — Strongest open-source Tetris AI
- **[libtetris](https://github.com/MinusKelvin/cold-clear)** — Tetris types (Board, Piece, FallingPiece) from the cold-clear workspace
- **[eframe/egui](https://github.com/emilk/egui)** — Immediate-mode GUI for the control panel
- **[windows](https://crates.io/crates/windows)** — Win32 API bindings (layered windows, GDI, keyboard state)

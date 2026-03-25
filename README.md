# TETR.IO Bot

A high-performance, stealth-oriented TETR.IO bot written in Rust. It runs as a separate process alongside the official TETR.IO Desktop client — reading the game state via screen capture and sending moves via simulated keyboard input.

## Architecture

The bot does **not** modify the TETR.IO client in any way. Instead it operates externally:

1. **Screen Capture** — Grabs frames from the desktop using the DXGI Desktop Duplication API (zero-copy, GPU-accelerated)
2. **Computer Vision** — Samples pixel colors at known grid positions and classifies them via HSV hue ranges to reconstruct the full board state, hold piece, next queue, and active piece
3. **AI Engine** — Feeds the reconstructed game state into Cold Clear (the strongest open-source Tetris AI) which computes the optimal placement
4. **Input Simulation** — Translates the AI's move into a sequence of keyboard inputs sent via Windows `SendInput`, with humanized timing (jitter, PPS limiting, thinking pauses, DAS/ARR-aware delays)

```
┌─────────────────┐     DXGI      ┌──────────────┐    HSV     ┌─────────────┐
│  TETR.IO Client  │──────────────▶│ Screen Capture│──────────▶│ Board Reader │
│  (unmodified)    │               │  (capture/)   │  pixels   │  (vision/)   │
└─────────────────┘               └──────────────┘           └──────┬──────┘
        ▲                                                           │ GameState
        │  SendInput                                                ▼
┌───────┴────────┐    MoveInputs   ┌──────────────┐  Pieces  ┌─────────────┐
│ Input Sender    │◀───────────────│  AI Engine    │◀─────────│ Cold Clear  │
│  (input/)       │   humanized    │  (ai/)        │  moves   │  (external) │
└────────────────┘                └──────────────┘           └─────────────┘
```

## File Structure

### Core Modules (`src/`)

| File | Purpose |
|------|---------|
| `main.rs` | Entry point. Spawns the bot loop on a background thread and runs the eframe/egui GUI on the main thread. Manages shared state (enabled/paused/PPS/pieces placed) between the bot thread and GUI via `Arc<BotState>`. Polls F9/F10 hotkeys via `GetAsyncKeyState`. |
| `capture/mod.rs` | Wraps `dxgi-capture-rs` (`DXGIManager`) for screen capture. `Frame` struct holds raw BGRA pixel data with `pixel_rgb(x, y)` and `crop()` methods. Caches the last successful frame so DXGI timeouts (no screen change) return stale data instead of erroring. |
| `vision/mod.rs` | Computer vision / board reading. `BoardReader` samples pixel centers of each cell in the 10x20 grid, hold box, and 5-slot next queue. `classify_color()` converts RGB → HSV and matches hue ranges calibrated from actual TETR.IO default skin pixel samples. Detects the active piece by scanning top rows, and infers game-active state from bottom rows. |
| `ai/mod.rs` | Wraps the Cold Clear AI engine (`cold_clear::Interface`). Translates `CellColor` → `libtetris::Piece`, feeds pieces incrementally (tracking `pieces_fed` to avoid duplicates), calls `suggest_next_move()` + `block_next_move()`, then converts Cold Clear's `PieceMovement` sequence (Left/Right/Cw/Ccw/SonicDrop) into our `MoveInput` enum. Supports 4 playstyles (balanced/aggressive/tspin/defensive) with tuned evaluator weights. |
| `input/mod.rs` | Sends keyboard inputs to TETR.IO via Windows `SendInput` API using `KEYBDINPUT` with scan codes. Humanization features: Gaussian jitter on inter-key delay and key hold duration, PPS rate limiter (default 4.0–6.5 PPS), random thinking pauses (12% chance, up to 180ms), ARR timing for repeated lateral moves, direction-switch hesitation. |
| `config/mod.rs` | Loads all settings from `config.toml` via serde. Structs: `CaptureConfig` (region, FPS), `VisionConfig` (board/hold/next pixel coordinates, cell size), `AiConfig` (playstyle), `InputConfig` (timing, humanization, keybinds), `HotkeyConfig` (toggle/pause keys). All fields have sensible defaults matching TETR.IO's default keybinds. |

### Calibration Tools (`src/bin/`)

| File | Purpose |
|------|---------|
| `calibrate.rs` | Live calibration tool — captures a DXGI screenshot and saves it as PNG. Subcommands: `sample` (read pixel color at coordinates), `grid` (draw board overlay), `scan` (edge detection). Requires TETR.IO to be visible on screen. |
| `analyze.rs` | Offline analysis tool — reads a previously saved PNG file (no live capture needed). Subcommands: `find` (auto-detect board edges), `sample` (pixel color), `grid` (overlay), `hscan`/`vscan` (brightness transition scanning). Used to verify and fine-tune calibration from VS Code without needing TETR.IO in the foreground. |

### Configuration

| File | Purpose |
|------|---------|
| `config.toml` | All tunable parameters. Calibrated for 1920x1080 borderless windowed, default TETR.IO skin, minimal graphics. Board grid starts at pixel (733, 86) with 45px cells. Hold piece sampled at (618, 190). Next queue at 5 positions along x=1310. |
| `Cargo.toml` | Rust dependencies and build config. Key crates: `dxgi-capture-rs` (screen capture), `cold-clear` + `libtetris` (AI), `windows` (SendInput), `eframe` (GUI), `rand`/`rand_distr` (humanization). Release profile uses LTO + single codegen unit for max performance. |

## Dependencies

- **[dxgi-capture-rs](https://crates.io/crates/dxgi-capture-rs)** — DXGI Desktop Duplication wrapper for zero-copy screen capture
- **[cold-clear](https://github.com/MinusKelvin/cold-clear)** — Strongest open-source Tetris AI (archived Jan 2024, still functional)
- **[libtetris](https://github.com/MinusKelvin/cold-clear)** — Tetris types (Board, Piece, FallingPiece, PieceMovement) from the cold-clear workspace
- **[eframe/egui](https://github.com/emilk/egui)** — Immediate-mode GUI for the control panel
- **[windows](https://crates.io/crates/windows)** — Official Microsoft Windows API bindings (SendInput, GetAsyncKeyState, MapVirtualKeyW)

## How It Works

### Color Classification (Vision)

Pixels are converted from RGB to HSV. Classification uses calibrated hue ranges from actual TETR.IO screenshots:

| Piece | Color | Hue Range |
|-------|--------|-----------|
| Z | Red | 345°–360°, 0°–15° |
| L | Orange | 16°–45° |
| O | Yellow | 46°–75° |
| S | Green | 76°–150° |
| I | Cyan | 151°–195° |
| J | Blue | 196°–265° |
| T | Purple | 266°–344° |

Brightness < 30 → Empty. Saturation < 0.20 → Garbage (if bright) or Empty.

### AI Integration (Cold Clear)

Cold Clear runs on background threads. The bot feeds it pieces incrementally as they become visible in the queue. On each board change:

1. Feed any new pieces (current + next queue) via `add_next_piece()`
2. Call `suggest_next_move(incoming_garbage)` to request computation
3. Call `block_next_move()` to wait for the result
4. Call `play_next_move(expected_location)` to advance Cold Clear's internal state
5. Convert the resulting `PieceMovement` list to keyboard inputs

### Humanization (Input)

To mimic human play patterns:
- **PPS limiting**: Each piece placement is throttled to 4.0–6.5 pieces per second
- **Gaussian jitter**: Inter-key delay (28ms ± 8ms) and key hold time (18ms ± 5ms) vary naturally
- **Thinking pauses**: 12% chance of a 20–180ms pause before starting inputs
- **DAS/ARR timing**: Repeated lateral moves use ARR delay; direction switches add hesitation
- **All timing is configurable** via `config.toml`

## Controls

| Key | Action |
|-----|--------|
| F9 | Start / Stop the bot |
| F10 | Pause / Resume (while running) |

The GUI window (always-on-top) shows status, pieces placed, current PPS, and has clickable Start/Stop/Pause buttons.

## Setup

1. Install Rust: https://rustup.rs
2. Clone and build:
   ```
   git clone https://github.com/lem-sleep/tetris_bot.git
   cd tetris_bot
   cargo build --release
   ```
3. Open TETR.IO Desktop in **1920x1080 borderless windowed** mode with **default skin** and **minimal graphics**
4. Run the bot:
   ```
   cargo run --release
   ```
5. Start a Zen mode game, then press **F9** to activate

### Recalibration

If your screen layout differs (different resolution, skin, or window position), use the calibration tools:

```
cargo run --bin calibrate -- sample 960 540    # Check pixel color at coordinates
cargo run --bin analyze -- find screenshot.png  # Auto-detect board edges from a saved screenshot
```

Then update the coordinates in `config.toml`.

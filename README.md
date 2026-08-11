# Serverless P2P Minesweeper

Multiplayer Minesweeper in Rust + WebAssembly. No game server, no accounts, no
database — peers connect directly over WebRTC DataChannels and stay in sync by
exchanging an event log.

**Status:** phase 1 (single-player) works. Phases 2 and 3 are not started.

## Layout

    engine/   pure Rust. Zero dependencies, no web types, no I/O.
    shell/    wasm-bindgen + canvas. Draws the engine, feeds it input.

The engine never imports anything web-shaped. That is what lets the renderer be
swapped for wgpu later without touching game logic, and what lets the tests run
natively in under a second.

## Design

The event log is the source of truth; board state is derived by folding
`Event`s over a fresh board. Mines are placed on the first click, seeded from
`(seed, first_click)` — both of which are in the log, so two peers derive
identical boards without exchanging them. `Game::hash()` reduces the whole
board to a `u64` for divergence detection.

## Develop

    cargo test --workspace       # engine tests, native, fast
    cd shell && trunk serve      # http://127.0.0.1:8080

`index.html` lives in `shell/`, not the repo root: trunk cannot build from a
virtual workspace with no root package.

## Roadmap

- **Phase 1** — deterministic engine, canvas renderer, mouse input. *Done.*
- **Phase 2** — WebRTC DataChannel co-op, signalling by QR/clipboard, desync
  detection. Serverless except for STUN; some networks will need a TURN relay
  and are not supported.
- **Phase 3** — wgpu renderer behind the same interface.

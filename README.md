# Serverless P2P Minesweeper

Multiplayer Minesweeper in Rust + WebAssembly. No game server, no accounts, no
database — peers connect directly over WebRTC DataChannels and stay in sync by
exchanging an event log.

**Status:** co-op multiplayer works, phone to laptop, over the open internet.
Play at <https://renanlopes77.github.io/minesweeper/>.

Press **Host**, scan the QR with the other device — it answers automatically —
then send its reply link back and paste it. Two links, no accounts, no server.

## Layout

    engine/   pure Rust. Zero dependencies, no web types, no I/O.
    shell/    wasm-bindgen + canvas. Draws the engine, feeds it input,
              and carries the event log over a WebRTC DataChannel.

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

## Signalling

The SDP travels in the URL **fragment** (`#o=` offer, `#a=` answer), base64url
encoded. Fragments are never sent to the server, so the handshake stays between
the two peers even though the page is hosted on GitHub Pages.

A public STUN server is the one piece of infrastructure this cannot avoid:
peers behind NAT need it to learn their own public address. It sees an IP and
nothing else. Networks requiring a TURN relay are not supported — roughly the
symmetric-NAT cases — and will simply fail to connect.

## Roadmap

- **Phase 1** — deterministic engine, canvas renderer, mouse input. *Done.*
- **Phase 2** — WebRTC co-op. Wire format, handshake, link + QR signalling,
  and event-log sync are *done*. Still open: move ordering under simultaneous
  clicks, desync detection via `Game::hash()`, and reconnect.
- **Phase 3** — wgpu renderer behind the same interface. Not started.

## Known gaps

- Simultaneous clicks are not ordered. Reveals are idempotent and flag toggles
  commute, so both sides still converge — but that is an argument, not a test.
- Nothing compares `Game::hash()` between peers yet, so a divergence would go
  unnoticed rather than being reported.
- A dropped channel ends the session; there is no reconnect.
- `wasm-opt` is disabled — see the comment in `.github/workflows/ci.yml`.

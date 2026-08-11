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
- **Phase 2** — WebRTC co-op. *Done.* Wire format, handshake, link + QR
  signalling, event-log sync, and desync detection via `Game::hash()`.
- **Phase 3** — make it pleasant. **Next.** Everything so far has been aimed
  at "does it work at all"; this is the pass that makes it a game someone
  would choose to play.

  *Connecting*
  - One contextual action instead of Host / Accept pasted / Copy sitting side
    by side with no indication of which to press or when.
  - Say what to do next at each step, and what state the connection is in.
  - Auto-copy the link when it is generated; auto-accept a pasted reply.
  - Hide the whole signalling panel once connected, show who is here instead.

  *The game*
  - Choose board size and mine count. `Event::Start` already carries `w`,
    `h`, and `mines` — nothing but the UI is missing.
  - A lobby: pick difficulty before hosting, so both sides agree up front.
  - Deliberate restart button rather than "click anywhere on a dead board".
  - Show mines remaining, and a timer.

  *Touch*
  - **Flagging is impossible on a phone.** Input is `mousedown` + `button()`,
    and there is no right-click on touch. Long-press, or a flag-mode toggle.
  - The canvas is a fixed 288px; it should scale to the viewport.

  *Presence*
  - `Event::player` is carried in every event and never used. Show who
    revealed what, and where the other player is looking.

- **Phase 4** — harden the sync. Deferred until the game is worth playing;
  the failures it addresses are real but rare, and now reported rather than
  silent.
  - Total-order events by `(seq, player)` so the three ordering hazards
    below cannot fire.
  - Reconnect: channel drops, re-signal, compare log lengths, ship the
    missing tail.

- **Phase 5** — a wgpu renderer, *if it ever earns its place.* Parked, and
  possibly permanently.

  Minesweeper is a static grid that changes only on click. Canvas2D draws a
  few thousand cells in well under a millisecond, and there is no frame loop
  because there is nothing to animate. wgpu would mean several hundred lines
  of adapter/device/surface/pipeline setup plus a texture atlas to replace
  `fill_text` — to produce the same picture, with more to break on a phone.

  Reasons that would change the answer: boards large enough that per-cell
  CPU work matters, smooth zoom and pan over them, or effects (explosions,
  shader transitions) that canvas cannot do well. Wanting to learn wgpu also
  counts — but as a learning project, not as something this game needs.

  The engine/shell split means this stays cheap to reconsider: the renderer
  is the only thing that would change, and every engine test would still
  pass. That was worth designing for even if it is never used.

## Known gaps

**Flagging does not work on touch devices.** Input reads `mousedown` and
`button()`, and there is no right-click on a phone. Every tap reveals.

**Three ordering hazards.** Each peer applies its own move before hearing the
other's, so the two logs are permutations. That is usually harmless — reveals
commute once the board exists, and flags always commute — but three cases
genuinely diverge. All three have tests in `engine/src/game.rs` asserting the
divergence, so they are documented rather than assumed away:

1. *The opening move.* Mines are placed on the first `Reveal`, seeded from
   that cell. Two peers who each open a cell before hearing from the other are
   playing different boards, not merely disagreeing about one square.
2. *Reveal and flag on the same cell.* Reveal-then-flag opens it and ignores
   the flag; flag-then-reveal leaves it flagged and opens nothing, because
   `reveal` skips anything that is not `Hidden`.
3. *A losing move.* `apply` ignores everything once the game is over, so a
   mine hit first silences what came after it, and a mine hit last does not.

The peers exchange a board hash after every move and report `DESYNC` when they
differ, so these are visible when they happen. Preventing them is phase 4.

**No reconnect.** A dropped channel ends the session.

**`wasm-opt` is disabled** — see the comment in `.github/workflows/ci.yml`.

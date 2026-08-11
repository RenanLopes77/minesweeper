# Serverless P2P Minesweeper

Multiplayer Minesweeper in Rust + WebAssembly. No game server, no accounts, no
database — peers connect directly over WebRTC DataChannels and stay in sync by
exchanging an event log.

**Status:** co-op multiplayer works, phone to laptop, over the open internet.
Play at <https://renanlopes77.github.io/minesweeper/>.

Press **Host** — the link is copied for you. Scan the QR with the other device
and it answers automatically; send its reply link back and paste it, which
connects on the spot. Two links, no accounts, no server.

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

Each event travels `Stamped` with a Lamport `seq`. Both peers keep the log
sorted by `(seq, event)` and refold the board from it, so the two of them agree
on one order no matter who heard what first — the hash is now a check on that
agreement rather than the only thing standing between you and a silent split.

## Develop

    cargo test --workspace       # engine tests, native, fast
    cd shell && trunk serve      # http://127.0.0.1:8080
    cd e2e && npx playwright test  # two real browsers, real WebRTC

The end-to-end tests are the only thing that exercises the DOM wiring and the
handshake: two pages swap links exactly as a human would, then play. They
compare `debug_hash()` — the board hash both peers are supposed to agree on —
which is exported from the shell for this and nothing else. First run needs
`npm install && npx playwright install chromium` in `e2e/`.

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

  *Connecting.* **Done.** Host and Join side by side — Join reads the link
  straight off the clipboard, so a phone never needs long-press-paste; links are
  copied to the clipboard the moment they exist; anything pasted into the box
  is acted on without a second press; the panel disappears once connected and
  the status line says which player you are.

  *The game.* Beginner / Intermediate / Expert from a picker, and a New game
  button that starts one — the `Start` travels, so whoever presses it sets the
  difficulty for both. Clicking a dead board no longer restarts it. The canvas
  resizes itself to whatever the board became. Above it, flags left and a clock
  that starts on the first reveal and freezes at game over.

  *Touch.* **Done.** A flag-mode toggle replaces the missing right button, and
  the canvas is a 2x bitmap scaled by CSS to `min(92vw, 20rem)` — downscaled
  rather than blown up, so it stays sharp on a phone. Long-press flagging is
  still unbuilt; the toggle covers it without a timer.

  *Presence.* **Done.** `Event::player` finally does something: flags are drawn
  in the colour of whoever planted them, the other player's last move is ringed
  in theirs, and the HUD says which colour you are. All of it is folded out of
  the log at draw time — no extra state to drift, no cursor streaming over the
  channel.

- **Phase 4** — harden the sync.
  - Total order. **Done.** Every event is `Stamped` with a Lamport `seq`, and
    the log is kept sorted by `(seq, event)` — the event itself breaks ties,
    so two peers moving at the same instant still agree. Arriving events are
    merged into place and the board is refolded from the whole log, which is
    why order of arrival no longer matters. The three hazards that used to
    diverge — the opening move, reveal-versus-flag on one cell, and a losing
    move — cannot fire now that both sides fold the same sequence.
  - Reconnect. **Done.** A drop — the channel closing, or the connection
    failing outright — brings the handshake panel back with the board and its
    log intact. `disconnected` gets five seconds of grace first, because ICE
    reports it on a blip and usually recovers by itself. Hosting or pasting a new link reconnects, and both sides then
    hand over their whole log: merging two logs of the same game *is* shipping
    the missing tail, in both directions, without either side working out what
    the other lacks.

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

**Reconnecting needs a new handshake.** The link is single-use, so coming back
means one more link exchange — there is no signalling channel left over to do
it silently.

**A peer's disappearance takes a while to notice.** Nothing is sent when a tab
simply closes, so the browser only reports the connection dead once its ICE
consent checks time out — up to about half a minute. The status line says the
other player has gone quiet as soon as the state wobbles, rather than leaving
a board that has silently stopped moving.

**The clock is per-device.** It starts at your first reveal, so someone who
joins a game in progress sees their own elapsed time, not the host's. Nothing
in the log records when the game began.

**`wasm-opt` is disabled** — see the comment in `.github/workflows/ci.yml`.

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

## Modes

Picked next to the difficulty, and carried on `Event::Start`, so both peers
switch together the moment someone presses New game.

**Co-op** — one board, cleared together. A mine ends it for both of you.

**Flag race** — one board, but the mines are the prize. Uncovering one *claims*
it in your colour instead of killing you, and the game ends when the last one
is taken; most claims wins. There is no way to lose a turn, so it is a race
rather than a standoff — the MSN Messenger rule.

**Race** — the same deal, a board each. Your moves land only on your copy, the
HUD shows both scores, and the first one home wins; stepping on a mine hands it
over. The layout cannot depend on who opened where, so mines are dealt around
the **centre cell** rather than around your first click: the middle is the safe
opening for both of you, and everywhere else is an honest risk.

One log still carries all of it. A race just folds that log twice — `race_fold`
in the engine, your events onto your board and theirs onto theirs — which is
also why the two peers compare a hash of the *log* in that mode instead of the
board: their boards are supposed to differ.

## Design

The event log is the source of truth; board state is derived by folding
`Event`s over a fresh board. Mines are placed on the first click, seeded from
`(seed, first_click)` — both of which are in the log, so two peers derive
identical boards without exchanging them. `Game::hash()` reduces the whole
game to a `u64` for divergence detection — the cells, but also the deal they
came from and the flag-race scoreboard, because two peers can hold identical
cells while playing different games.

Each event travels `Stamped` with a Lamport `seq` and the author's `at_ms`.
Both peers keep the log sorted by `(seq, event)` and refold the board from it,
so the two of them agree on one order no matter who heard what first — the hash
is now a check on that agreement rather than the only thing standing between
you and a silent split. `at_ms` never affects the order; it is what lets the
game clock be read out of the log, so a peer who joins halfway through shows
the same elapsed time as everyone else instead of starting from zero.

## Develop

    cargo test --workspace       # engine tests, native, fast
    cd shell && trunk serve      # http://127.0.0.1:8080
    cd e2e && npx playwright test  # two real browsers, real WebRTC
    BASE_URL=https://renanlopes77.github.io/minesweeper/ npx playwright test

The end-to-end tests are the only thing that exercises the DOM wiring and the
handshake: two pages swap links exactly as a human would, then play. They
compare `debug_hash()` — the board hash both peers are supposed to agree on —
which is exported from the shell for this and nothing else. First run needs
`npm install && npx playwright install chromium` in `e2e/`.

`BASE_URL` points the same suite at a deployed site instead of a local build —
production is the only place the real STUN path and GitHub Pages' own caching
are exercised. Tests navigate with `goto('.')` rather than `'/'` so a site
served under a project path is reachable.

`index.html` lives in `shell/`, not the repo root: trunk cannot build from a
virtual workspace with no root package.

## Signalling

The SDP travels in the URL **fragment** (`#o=` offer, `#a=` answer), deflated
and then base64url encoded. Fragments are never sent to the server, so the
handshake stays between the two peers even though the page is hosted on GitHub
Pages.

Deflate is worth it because the link is carried by a human: an SDP is
repetitive ASCII, so `CompressionStream('deflate-raw')` — a browser feature,
not a dependency — takes an offer from 994 bytes to 568, the link from 1372
characters to 783, and the QR code from 141 modules square to 109. Anything
that cannot be compressed is sent plain, and a link from an older peer that
was never compressed still decodes.

**Two links is not a choice.** The peer answering has to send back its DTLS
fingerprint, its ICE credentials and at least one candidate; none of the three
can be guessed in advance, and browsers reject an SDP whose fingerprint or
credentials have been rewritten. Libraries that manage one round trip —
[wasm-peers](https://github.com/wasm-peers/wasm-peers),
[matchbox](https://github.com/johanhelsing/matchbox) — all do it by running a
signalling server that both peers can reach. That is the trade: a server, or a
second link.

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
  that starts on the first reveal and freezes at game over. The result is a
  line of text above the board rather than a banner painted across the middle
  of it, and once it is over the flags you got wrong are crossed out.

  *Touch.* **Done.** A flag-mode toggle replaces the missing right button, and
  the canvas is a 2x bitmap that CSS scales down, so it stays sharp on a phone.
  The board is sized by cell count rather than by a fixed width — `--cols` and
  `--rows` are set from Rust, and CSS takes whichever of *96vw*, *34px a cell*
  and *70vh* bites first. Expert therefore fills a desktop without overflowing
  it and Beginner does not stretch to match. Long-press flagging is still
  unbuilt; the toggle covers it without a timer.

  30 columns on a phone is about 16px a cell whatever the CSS says. Pinch-zoom
  and pan is the only real fix and is not built.

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
    log intact, and it does not matter which side hosts the reconnect: a player
    who has already moved keeps that seat, and the game with moves in it is the
    one that survives the meeting. `disconnected` gets five seconds of grace first, because ICE
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

**The clock stops at the move that ended the game**, not at the last event in
the log — peers keep sending moves until they hear the bad news, and those
merge in behind the losing click.

**The running clock is only as good as the two devices' clocks.** It counts
from the timestamp on the first reveal, which was written by whoever made that
move, so peers whose system clocks disagree will disagree by that much while
the game is live. The *final* time is subtracted entirely out of the log —
last event minus first reveal — so it is identical on both screens.

**`wasm-opt` is disabled** — see the comment in `.github/workflows/ci.yml`.

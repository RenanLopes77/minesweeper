import { test, expect } from '@playwright/test';

// Two real pages, a real DataChannel, and the same link exchange a human
// would do. Everything the unit tests cannot reach — the DOM wiring and the
// handshake — is only ever exercised here.

const CELL = 32; // logical px; the canvas is 2x that and CSS-scaled

/// Clicks a board cell by its grid coordinates, whatever the canvas is scaled to.
async function clickCell(page, cx, cy, button = 'left') {
  const board = page.locator('#board');
  const box = await board.boundingBox();
  const size = await board.evaluate((c) => c.width / 2); // logical px across
  const scale = box.width / size;
  await board.click({
    button,
    position: { x: (cx + 0.5) * CELL * scale, y: (cy + 0.5) * CELL * scale },
  });
}

const hash = (page) => page.evaluate(() => window.wasmBindings.debug_hash());
const logLen = (page) => page.evaluate(() => window.wasmBindings.debug_log_len());

/// Host on `a`, join on `b`, and come back once both say they are connected.
/// `reload` is false when `a` is already mid-game — navigating would throw
/// away the very log the reconnect is supposed to preserve.
async function connect(a, b, { reload = true } = {}) {
  if (reload) await a.goto('/');
  await a.locator('#go').click();
  await expect(a.locator('#sig')).not.toHaveValue('', { timeout: 30_000 });
  const offer = await a.locator('#sig').inputValue();

  // Opening the link is the joiner's whole job: the page answers by itself.
  await b.goto(offer);
  await expect(b.locator('#sig')).not.toHaveValue('', { timeout: 30_000 });
  const reply = await b.locator('#sig').inputValue();

  // fill() fires `input`, which is what the page listens for.
  await a.locator('#reply').fill(reply);
  await expect(a.locator('#status')).toContainText('connected', { timeout: 30_000 });
  await expect(b.locator('#status')).toContainText('connected', { timeout: 30_000 });
}

/// Expert is 30 cells wide and Beginner is 9. Sizing the canvas by a fixed
/// width squashed one or stretched the other; it is sized by cell count now,
/// capped by both viewport dimensions.
test('the board fits the window at every difficulty', async ({ page }) => {
  await page.goto('/');
  const box = async () => page.locator('#board').boundingBox();
  const view = page.viewportSize();

  for (const [index, cols, rows] of [
    [0, 9, 9],
    [1, 16, 16],
    [2, 30, 16],
  ]) {
    await page.locator('#level').selectOption({ index });
    await page.locator('#restart').click();
    const b = await box();

    expect(b.width).toBeLessThanOrEqual(view.width * 0.96 + 1);
    expect(b.height).toBeLessThanOrEqual(view.height * 0.7 + 1);
    // Square cells: the shape on screen matches the shape of the board.
    expect(b.width / b.height).toBeCloseTo(cols / rows, 1);
    // And never bigger than 34px a cell, however much room there is.
    expect(b.width / cols).toBeLessThanOrEqual(34.5);
  }

  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
});

test('two peers play the same board', async ({ browser }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();
  await connect(a, b);

  // The flag goes down first, on a board that is still entirely hidden — a
  // reveal can flood over any given cell, and flagging an open cell is a
  // no-op, which would make this assertion depend on the seed.
  await clickCell(b, 8, 0, 'right');
  await clickCell(a, 4, 4);
  await clickCell(b, 0, 8);

  await expect.poll(() => logLen(a)).toBe(4);
  await expect.poll(() => logLen(b)).toBe(4);
  expect(await hash(a)).toBe(await hash(b));
  expect(await hash(a)).not.toBe('');

  // The flag is on both boards, so the counter moved on both.
  await expect(a.locator('#hud')).toContainText('9 flags left');
  await expect(b.locator('#hud')).toContainText('9 flags left');

  await a.close();
  await b.close();
});

test('a move made before the peer is heard from still converges', async ({ browser }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();
  await connect(a, b);

  // Both open a cell in the same tick: this is hazard 1 — the opening move —
  // which used to leave the two of them on different mine layouts.
  await Promise.all([clickCell(a, 0, 0), clickCell(b, 8, 8)]);

  await expect.poll(() => logLen(a)).toBe(3);
  await expect.poll(() => logLen(b)).toBe(3);
  await expect.poll(async () => (await hash(a)) === (await hash(b))).toBe(true);
  await expect(a.locator('#status')).not.toContainText('DESYNC');
  await expect(b.locator('#status')).not.toContainText('DESYNC');

  await a.close();
  await b.close();
});

test('losing the peer restores the handshake and keeps the board', async ({ browser }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();
  await connect(a, b);

  await clickCell(a, 4, 4);
  await expect.poll(() => logLen(b)).toBe(2);
  const before = await hash(a);

  await b.close(); // the peer walks away

  await expect(a.locator('#status')).toContainText('connection lost', { timeout: 30_000 });
  await expect(a.locator('#handshake')).toBeVisible();
  await expect(a.locator('#go')).toHaveText('Host a game');
  expect(await hash(a)).toBe(before); // the board survived the drop

  await a.close();
});

/// A New game is a move, not a game being handed over. If a peer mistakes it
/// for one it replaces its whole log, the two counts drift apart forever, and
/// `Msg::State` stops comparing hashes — desync detection off, silently.
test('a restart keeps both logs the same length', async ({ browser }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();
  await connect(a, b);

  await clickCell(a, 4, 4);
  await expect.poll(() => logLen(b)).toBe(2);

  await a.locator('#level').selectOption({ index: 1 }); // Intermediate
  await a.locator('#restart').click();

  await expect.poll(() => logLen(a)).toBe(3);
  await expect.poll(() => logLen(b)).toBe(3);
  expect(await hash(a)).toBe(await hash(b));
  await expect(b.locator('#hud')).toContainText('40 flags left');

  await a.close();
  await b.close();
});

/// Pasting a link into the address bar of a tab that already has the game open
/// is a fragment-only navigation: the page never reloads, so the offer has to
/// be picked up from `hashchange` rather than from startup.
test('a link pasted into an already-open tab still joins', async ({ browser }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();

  await a.goto('/');
  await a.locator('#go').click();
  await expect(a.locator('#sig')).not.toHaveValue('', { timeout: 30_000 });
  const offer = await a.locator('#sig').inputValue();

  await b.goto('/'); // open first, *then* receive the link
  await b.evaluate((url) => {
    window.location.href = url;
  }, offer);

  await expect(b.locator('#sig')).not.toHaveValue('', { timeout: 30_000 });
  await a.locator('#reply').fill(await b.locator('#sig').inputValue());
  await expect(a.locator('#status')).toContainText('connected', { timeout: 30_000 });
  await expect(b.locator('#status')).toContainText('connected', { timeout: 30_000 });

  await a.close();
  await b.close();
});

test('a fresh joiner adopts the host board, mid-game', async ({ browser }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();
  await connect(a, b);

  await clickCell(a, 4, 4);
  await clickCell(a, 1, 1, 'right');
  await expect.poll(() => logLen(b)).toBe(3);
  await b.close();
  await expect(a.locator('#status')).toContainText('connection lost', { timeout: 30_000 });

  // Someone new joins the game already in progress and gets the whole log.
  const c = await browser.newPage();
  await connect(a, c, { reload: false });
  await expect.poll(() => logLen(c)).toBe(3);
  expect(await hash(c)).toBe(await hash(a));

  // The clock came out of the log, so the newcomer is not starting from zero:
  // it reads the same elapsed time as the player who has been here all along.
  const secs = async (p) => Number((await p.locator('#hud').textContent()).match(/(\d+)s/)[1]);
  await expect.poll(async () => Math.abs((await secs(a)) - (await secs(c)))).toBeLessThanOrEqual(1);
  expect(await secs(c)).toBeGreaterThan(0);

  await a.close();
  await c.close();
});

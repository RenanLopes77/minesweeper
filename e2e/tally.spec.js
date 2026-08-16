import { test, expect } from '@playwright/test';

// The plug-and-play proof for the extracted crates: a page that is not the
// game — demo-counter, eventlog + p2p-link and nothing else — connects and
// converges through the same handshake the game uses.

// The demo is not deployed anywhere, so a BASE_URL run has nothing to test.
test.skip(!!process.env.BASE_URL, 'demo only exists in the local build');

test.use({ baseURL: 'http://127.0.0.1:8082' });

test('two tally pages connect and agree', async ({ context }) => {
  const a = await context.newPage();
  const b = await context.newPage();

  await a.goto('.');
  await a.locator('#go').click();
  await expect(a.locator('#sig')).not.toHaveValue('', { timeout: 30_000 });
  const offer = await a.locator('#sig').inputValue();

  // Opening the link is the joiner's whole job: the page answers by itself.
  await b.goto(offer);
  await expect(b.locator('#sig')).not.toHaveValue('', { timeout: 30_000 });
  const reply = await b.locator('#sig').inputValue();

  await a.locator('#reply').fill(reply);
  await expect(a.locator('#status')).toContainText('connected', { timeout: 30_000 });
  await expect(b.locator('#status')).toContainText('connected', { timeout: 30_000 });

  await a.locator('#tap').click();
  await a.locator('#tap').click();
  await b.locator('#tap').click();

  await expect(a.locator('#tally')).toHaveText('you 2 — 1 them   (3 total)');
  await expect(b.locator('#tally')).toHaveText('you 1 — 2 them   (3 total)');
});

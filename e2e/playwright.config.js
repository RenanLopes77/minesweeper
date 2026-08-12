import { defineConfig } from '@playwright/test';

// The app is static files. `trunk build` makes them; python serves them, so
// this needs no server dependency of its own.
// BASE_URL points the suite at a deployed site instead of a local build:
//   BASE_URL=https://renanlopes77.github.io/minesweeper/ npx playwright test
// Production is the only place the real STUN path, HTTPS clipboard rules and
// GitHub Pages' own caching are exercised.
// Normalise: a missing trailing slash makes new URL('.', base) drop the
// project path (…/minesweeper -> account root), and an empty string is not a
// site — both fall back to the local server rather than failing obscurely.
const LIVE = process.env.BASE_URL
  ? process.env.BASE_URL.replace(/\/?$/, '/')
  : undefined;

export default defineConfig({
  testDir: '.',
  timeout: 60_000,
  expect: { timeout: 20_000 },
  use: {
    baseURL: LIVE ?? 'http://127.0.0.1:8081',
    // The handshake needs a real ICE stack; headless Chromium has one.
    launchOptions: { args: ['--autoplay-policy=no-user-gesture-required'] },
  },
  // Nothing to serve when the site under test is already deployed.
  webServer: LIVE ? undefined : {
    command:
      'cd ../shell && trunk build && python3 -m http.server 8081 --bind 127.0.0.1 --directory dist',
    url: 'http://127.0.0.1:8081',
    reuseExistingServer: true,
    timeout: 180_000,
  },
});

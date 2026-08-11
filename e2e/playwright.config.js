import { defineConfig } from '@playwright/test';

// The app is static files. `trunk build` makes them; python serves them, so
// this needs no server dependency of its own.
export default defineConfig({
  testDir: '.',
  timeout: 60_000,
  expect: { timeout: 20_000 },
  use: {
    baseURL: 'http://127.0.0.1:8081',
    // The handshake needs a real ICE stack; headless Chromium has one.
    launchOptions: { args: ['--autoplay-policy=no-user-gesture-required'] },
  },
  webServer: {
    command:
      'cd ../shell && trunk build && python3 -m http.server 8081 --bind 127.0.0.1 --directory dist',
    url: 'http://127.0.0.1:8081',
    reuseExistingServer: true,
    timeout: 180_000,
  },
});

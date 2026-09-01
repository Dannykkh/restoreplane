import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  retries: process.env.CI ? 1 : 0,
  reporter: [["list"]],
  use: {
    baseURL: "http://127.0.0.1:1420",
    colorScheme: "dark",
    reducedMotion: "reduce",
    screenshot: {
      mode: "only-on-failure",
      fullPage: true,
    },
    trace: "retain-on-failure",
  },
  projects: [
    { name: "chromium-1024", use: { browserName: "chromium", viewport: { width: 1024, height: 768 } } },
    { name: "chromium-1280", use: { browserName: "chromium", viewport: { width: 1280, height: 800 } } },
    { name: "chromium-1600", use: { browserName: "chromium", viewport: { width: 1600, height: 900 } } },
  ],
  webServer: {
    command: "npm run dev",
    url: "http://127.0.0.1:1420",
    timeout: 120_000,
    reuseExistingServer: !process.env.CI,
  },
});

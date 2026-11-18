import { defineConfig } from "@playwright/test";
import base from "../playwright.config";

const run = process.env.RESTOREPLANE_UI_QA_RUN;
if (!run || !/^browser-\d+$/.test(run)) throw new Error("Use the preserved UI check runner with an explicit attempt");
const output = `D:/git/dbbackupsystem/artifacts/workbench-implementation/workbench-20261117-143715/ui-integration-20261118/${run}`;
const productionFixture = process.env.RESTOREPLANE_UI_QA_STATIC === "1";
export default defineConfig({
  ...base, testDir: ".", retries: 0, workers: 1,
  outputDir: `${output}-output`,
  reporter: [["list"], ["json", { outputFile: `${output}.results.json` }]],
  projects: [{ name: "chromium-1280", use: { browserName: "chromium", viewport: { width: 1280, height: 800 } } }],
  webServer: {
    command: productionFixture ? "node node_modules/vite/bin/vite.js preview --outDir D:/git/dbbackupsystem/artifacts/workbench-implementation/workbench-20261117-143715/ui-integration-20261118/offline-browser-build --host 127.0.0.1 --port 1420 --strictPort" : "node node_modules/vite/bin/vite.js --host 127.0.0.1 --port 1420 --strictPort",
    cwd: "D:/git/dbbackupsystem/apps/console",
    url: productionFixture ? "http://127.0.0.1:1420/tests/workbench-offline.html" : "http://127.0.0.1:1420", timeout: 120_000, reuseExistingServer: false,
    env: { TAURI_DEV_HOST: "" },
  },
});

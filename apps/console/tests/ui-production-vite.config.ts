import { defineConfig, mergeConfig } from "vite";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import base from "../vite.config";

const appRoot = fileURLToPath(new URL("../", import.meta.url));
const csp = JSON.parse(readFileSync(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8")).app.security.csp as string;
export default defineConfig(mergeConfig(base, {
  root: appRoot,
  plugins: [{ name: "test-production-policy", transformIndexHtml: { order: "pre", handler: (html: string) => html.replace("%RESTOREPLANE_CSP%", csp.replaceAll('"', "&quot;")) } }],
  build: {
    outDir: "D:/git/dbbackupsystem/artifacts/workbench-implementation/workbench-20261117-143715/ui-integration-20261118/offline-browser-build",
    emptyOutDir: false, manifest: true,
    rollupOptions: { input: fileURLToPath(new URL("./workbench-offline.html", import.meta.url)) },
  },
}));

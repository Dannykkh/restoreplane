import { expect, test, type Locator, type Page } from "@playwright/test";
import { writeFile } from "node:fs/promises";
import AxeBuilder from "@axe-core/playwright";

async function connect(page: Page, query = "") {
  await page.goto(`/tests/${process.env.RESTOREPLANE_UI_QA_STATIC === "1" ? "workbench-offline.html" : "workbench-mock.html"}${query}`);
  if (await page.getByLabel("대화형 연결 암호").count()) {
    await page.getByLabel("데이터베이스 사용자").fill("mock_reader");
    await page.getByLabel("대화형 연결 암호").fill("explicit-mock-only");
  }
  await page.getByRole("button", { name: "실제 대상 확인하고 연결", exact: true }).click();
  if (await page.locator(".wb-explorer-fold").getAttribute("open") === null) await page.locator(".wb-explorer-fold > summary").click();
  await page.getByRole("treeitem", { name: /main/ }).click();
  await page.getByRole("treeitem", { name: /테이블_00000/ }).click();
  await expect(page.getByRole("grid", { name: "조회 결과", exact: true })).toBeVisible();
}
const commands = (page: Page) => page.evaluate(() => (window as unknown as { workbenchMockRequests: { command: string; payload: Record<string, unknown> }[] }).workbenchMockRequests);
const editor = (page: Page) => page.getByRole("textbox", { name: "SQL 원문", exact: true });
async function openSql(page: Page) {
  await page.getByRole("button", { name: "SQL 작성·검토", exact: true }).click();
  await expect(editor(page)).toBeVisible();
}
async function exactPaste(target: Locator, text: string) {
  await target.focus(); await target.press("Control+a");
  await target.evaluate((node, value) => {
    const clipboardData = new DataTransfer(); clipboardData.setData("text/plain", value);
    node.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData }));
  }, text);
}
async function visibleArea(target: Locator) {
  return target.evaluate(node => {
    const r = node.getBoundingClientRect(); let left = Math.max(0, r.left), top = Math.max(0, r.top), right = Math.min(innerWidth, r.right), bottom = Math.min(innerHeight, r.bottom);
    for (let parent = node.parentElement; parent; parent = parent.parentElement) {
      const style = getComputedStyle(parent), p = parent.getBoundingClientRect();
      if (/(hidden|auto|scroll|clip)/.test(style.overflowX)) { left = Math.max(left, p.left); right = Math.min(right, p.right); }
      if (/(hidden|auto|scroll|clip)/.test(style.overflowY)) { top = Math.max(top, p.top); bottom = Math.min(bottom, p.bottom); }
    }
    return { x: r.x, y: r.y, width: r.width, height: r.height, visibleWidth: Math.max(0, right - left), visibleHeight: Math.max(0, bottom - top) };
  });
}

test("UI-01/11: tables dominate and the first SQL view exposes usable editor and result regions", async ({ page }, info) => {
  await connect(page);
  const grid = page.getByRole("grid", { name: "조회 결과", exact: true });
  const tableArea = await visibleArea(grid);
  const contextArea = await visibleArea(page.locator(".wb-context"));
  expect(await grid.getByRole("gridcell").count()).toBeLessThan(1000);
  expect(tableArea.visibleWidth * tableArea.visibleHeight).toBeGreaterThan(contextArea.visibleWidth * contextArea.visibleHeight);
  expect(tableArea.visibleHeight).toBeGreaterThanOrEqual(contextArea.visibleHeight * 2);
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
  await page.screenshot({ path: info.outputPath("table-dark.png"), fullPage: true });
  await openSql(page);
  const editArea = await visibleArea(page.locator(".cm-scroller"));
  const resultArea = await visibleArea(page.locator(".wb-sql-results-region"));
  await writeFile(info.outputPath("first-view-geometry.json"), JSON.stringify({ tableArea, contextArea, editArea, resultArea, scope: "Actual Chromium mock UI; table dominance compares visible data area with context controls, no invented200px product requirement or native database claim" }, null, 2));
  await page.screenshot({ path: info.outputPath("sql-first-dark.png"), fullPage: true });
  expect(editArea.visibleHeight).toBeGreaterThanOrEqual(120);
  expect(resultArea.visibleHeight).toBeGreaterThanOrEqual(100);
  await editor(page).fill("SELECT 1");
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await page.getByRole("button", { name: "검토한 SQL 실행", exact: true }).click();
  await expect(page.getByRole("grid", { name: "조회 결과", exact: true })).toBeVisible();
  await page.screenshot({ path: info.outputPath("sql-result-dark.png"), fullPage: true });
});

test("UI-03/05: exact mixed-line-ending clipboard input survives review and undo without automatic execution", async ({ page }, info) => {
  await connect(page); await openSql(page);
  const text = "-- 한글 😀\r\nSELECT 'quote'';\\';\nSELECT 2\r";
  await exactPaste(editor(page), text);
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await expect.poll(async () => (await commands(page)).filter(r => r.command === "query.review").at(-1)?.payload.sql).toBe(text);
  await editor(page).press("End"); await editor(page).press("x"); await editor(page).press("Control+z");
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await expect.poll(async () => (await commands(page)).filter(r => r.command === "query.review").at(-1)?.payload.sql).toBe(text);
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
  await writeFile(info.outputPath("exact-source-evidence.json"), JSON.stringify({ exactReviewPayload: true, undoRoundTrip: true, syntheticBrowserClipboard: true, physicalOsClipboard: "NOT RUN", nativeDatabase: "NOT RUN" }, null, 2));
});

test("UI-05/06/07: one CtrlSpace gesture inserts a quoted name and typing never requeries metadata", async ({ page }, info) => {
  await connect(page); await openSql(page);
  await editor(page).fill("SELECT 열_1");
  const metadata = async () => (await commands(page)).filter(r => r.command.startsWith("catalog.")).length;
  const before = await metadata();
  await editor(page).press("Control+Space");
  await expect(page.locator(".cm-tooltip-autocomplete")).toBeVisible();
  const afterExplicit = await metadata();
  expect(afterExplicit - before).toBeGreaterThan(0); expect(afterExplicit - before).toBeLessThanOrEqual(7);
  await editor(page).press("Enter");
  expect((await commands(page)).filter(r => ["query.review", "query.run"].includes(r.command))).toHaveLength(0);
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await expect.poll(async () => (await commands(page)).find(r => r.command === "query.review")?.payload.sql).toBe('SELECT "열_1"');
  await editor(page).fill("SELECT 열_2"); await editor(page).press("Control+Space");
  await expect(page.locator(".cm-tooltip-autocomplete")).toBeVisible();
  const beforeTyping = await metadata();
  await editor(page).press("x"); await editor(page).press("Backspace"); await editor(page).press("Delete"); await editor(page).press("Control+z");
  await page.waitForTimeout(400);
  expect(await metadata()).toBe(beforeTyping);
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
  await writeFile(info.outputPath("completion-evidence.json"), JSON.stringify({ explicitNativeCalls: afterExplicit - before, typingExtraNativeCalls: 0, enterExecutesSql: false, quotedInsertionExact: true }, null, 2));
});

test("UI-06/08: pending metadata cannot stack or appear after pane scope changes", async ({ page }) => {
  await connect(page); await openSql(page); await editor(page).fill("SELECT 열_");
  const metadata = async () => (await commands(page)).filter(r => r.command.startsWith("catalog.")).length;
  const before = await metadata();
  await page.evaluate(() => { (window as unknown as { workbenchMockControl: { metadataDelayMs: number } }).workbenchMockControl.metadataDelayMs = 600; });
  await editor(page).press("Control+Space");
  await expect.poll(metadata).toBe(before + 1);
  await editor(page).press("Control+Space");
  await page.getByRole("button", { name: "행 조회", exact: true }).click();
  await page.waitForTimeout(800);
  expect(await metadata()).toBe(before + 1);
  await expect(page.locator(".cm-tooltip-autocomplete")).toHaveCount(0);
  await openSql(page);
  await expect(page.locator(".cm-tooltip-autocomplete")).toHaveCount(0);
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
});

test("UI-06/07: unresolved qualifiers refuse metadata instead of guessing another schema", async ({ page }) => {
  await connect(page); await openSql(page);
  const before = (await commands(page)).filter(r => r.command.startsWith("catalog.")).length;
  for (const text of ["SELECT other_schema.열", "SELECT alias.\n열", "SELECT alias. 열", "SELECT '열", "SELECT alias./*x*/ 열"]) {
    await editor(page).fill(text); await editor(page).press("Control+Space"); await page.waitForTimeout(180);
    await expect(page.locator(".cm-tooltip-autocomplete")).toHaveCount(0);
  }
  expect((await commands(page)).filter(r => r.command.startsWith("catalog.")).length).toBe(before);
});

test("UI-04: aggregate eight-tab quota rejects one byte atomically and retains the previous review", async ({ page }, info) => {
  test.setTimeout(90_000);
  await connect(page); await openSql(page);
  const half = 512 * 1024;
  await exactPaste(editor(page), "x".repeat(half));
  await page.getByRole("button", { name: "새 SQL 탭", exact: true }).click();
  await exactPaste(editor(page), "y".repeat(half));
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await expect(page.getByRole("button", { name: "검토한 SQL 실행", exact: true })).toBeEnabled();
  await editor(page).press("Control+End"); await editor(page).press("z");
  await expect(page.getByRole("status").filter({ hasText: "열린 SQL 초안의 허용 범위를 넘었습니다" })).toBeVisible();
  await expect(page.getByRole("button", { name: "검토한 SQL 실행", exact: true })).toBeEnabled();
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await expect.poll(async () => String((await commands(page)).filter(r => r.command === "query.review").at(-1)?.payload.sql).length).toBe(half);
  await editor(page).press("Control+z");
  await expect(page.getByRole("button", { name: "검토한 SQL 실행", exact: true })).toBeDisabled();
  for (let i = 0; i < 6; i++) await page.getByRole("button", { name: "새 SQL 탭", exact: true }).click();
  await expect(page.getByRole("tab", { name: /쿼리/ })).toHaveCount(8);
  await expect(page.getByRole("button", { name: "새 SQL 탭", exact: true })).toBeDisabled();
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
  await writeFile(info.outputPath("aggregate-quota-evidence.json"), JSON.stringify({ aggregateAcceptedBytes: 1048576, extraByteRejected: true, rejectedEditRetainsReview: true, undoInvalidatesReview: true, tabCount: 8, automaticSql: 0 }, null, 2));
});

test("UI-05/10: composition gates shortcuts and a hidden row grid restores identity and both scroll axes", async ({ page }) => {
  await connect(page);
  const grid = page.getByRole("grid", { name: "조회 결과", exact: true });
  await grid.evaluate(node => { node.scrollTop = 352; node.scrollLeft = 552; node.dispatchEvent(new Event("scroll")); });
  await grid.getByRole("gridcell", { name: "열_4: 11:4", exact: true }).click();
  const scroll = await grid.evaluate(node => ({ top: node.scrollTop, left: node.scrollLeft, active: node.getAttribute("aria-activedescendant") }));
  await openSql(page); await editor(page).fill("SELECT '한글'");
  await expect(page.locator(".wb-rows-slot .wb-grid")).toHaveCount(0);
  await editor(page).dispatchEvent("compositionstart", { data: "한" });
  await editor(page).press("Control+Enter"); await editor(page).press("Control+Space");
  expect((await commands(page)).filter(r => ["query.review", "query.run"].includes(r.command))).toHaveLength(0);
  await editor(page).dispatchEvent("compositionend", { data: "한" });
  await page.getByRole("button", { name: "행 조회", exact: true }).click();
  await expect.poll(() => grid.evaluate(node => ({ top: node.scrollTop, left: node.scrollLeft, active: node.getAttribute("aria-activedescendant") }))).toEqual(scroll);
  await openSql(page); await editor(page).press("Control+Enter");
  await expect.poll(async () => (await commands(page)).filter(r => r.command === "query.review").length).toBe(1);
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
});

for (const engine of ["postgresql", "mysql", "mariadb", "sqlite", "unknown"]) test(`UI-02: ${engine} uses its dialect and fail-closed helper availability`, async ({ page }, info) => {
  await connect(page, `?engine=${engine}`); await openSql(page); await editor(page).fill("SELECT '한글', 42");
  const spans = page.locator(".cm-line span");
  if (engine === "unknown") {
    await expect(spans).toHaveCount(0);
    await expect(page.getByRole("button", { name: "실행 계획 검토", exact: true })).toBeDisabled();
  } else {
    await expect(spans.filter({ hasText: "SELECT" })).toBeVisible();
    await expect(page.getByRole("button", { name: "실행 계획 검토", exact: true })).toBeEnabled();
    await page.getByRole("button", { name: "실행 계획 검토", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "실행 계획 SQL을 검토할까요?" });
    await dialog.getByRole("button", { name: "이 SQL 전체 검토", exact: true }).click();
    const expected = `${engine === "sqlite" ? "EXPLAIN QUERY PLAN" : "EXPLAIN"} SELECT '한글', 42`;
    await expect.poll(async () => (await commands(page)).find(r => r.command === "query.review")?.payload.sql).toBe(expected);
  }
  await expect(page.getByRole("button", { name: "실행 분석 검토", exact: true })).toBeDisabled();
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
  await page.screenshot({ path: info.outputPath(`dialect-${engine}.png`), fullPage: true });
});

test("UI-03/09: explicit saved-query consent and reload preserve mixed source and transaction ownership", async ({ page }, info) => {
  await connect(page); await openSql(page);
  const source = "-- 한글 😀\r\nSELECT 'a''b';\nSELECT 2\r";
  await exactPaste(editor(page), source);
  await page.getByRole("button", { name: "이름을 정해 저장", exact: true }).click();
  const save = page.getByRole("dialog", { name: "SQL 이름을 정해 저장", exact: true });
  await save.getByLabel("저장할 이름").fill("혼합 원문");
  await expect(save.getByRole("button", { name: "암호화해서 저장", exact: true })).toBeDisabled();
  await save.getByRole("checkbox").check();
  await save.getByRole("button", { name: "암호화해서 저장", exact: true }).click();
  await expect(save).not.toBeVisible();
  expect((await commands(page)).find(r => r.command === "saved_query.put")?.payload.sql).toBe(source);
  await page.getByRole("button", { name: "새 SQL 탭", exact: true }).click();
  await page.getByRole("button", { name: "저장한 SQL 열기", exact: true }).click();
  const load = page.getByRole("dialog", { name: "저장한 SQL 열기", exact: true });
  await load.getByRole("button", { name: /혼합 원문/ }).click();
  await expect(load).not.toBeVisible();
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await expect.poll(async () => (await commands(page)).find(r => r.command === "query.review")?.payload.sql).toBe(source);
  await expect(page.getByRole("combobox", { name: "SQL 모드", exact: true })).toHaveCount(1);
  const before = (await commands(page)).filter(r => r.command === "transaction.get").length;
  await page.getByRole("button", { name: "행 조회", exact: true }).click(); await openSql(page);
  await page.waitForTimeout(4200);
  const polls = (await commands(page)).filter(r => r.command === "transaction.get").length - before;
  expect(polls).toBeGreaterThanOrEqual(2); expect(polls).toBeLessThanOrEqual(3);
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
  await writeFile(info.outputPath("saved-source-evidence.json"), JSON.stringify({ explicitConsent: true, exactSaveAndReloadReviewPayload: true, transactionControls: 1, pollsDuring4200ms: polls, nativeEncryption: "NOT RUN: explicit browser mock only" }, null, 2));
});

test("UI-09/10: Stop and Unknown acknowledgement remain reachable outside hidden SQL panes", async ({ page }, info) => {
  await connect(page); await openSql(page); await editor(page).fill("SELECT 1");
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await page.getByRole("button", { name: "검토한 SQL 실행", exact: true }).click();
  await expect(page.getByRole("button", { name: "현재 실행 중지", exact: true })).toBeEnabled();
  await expect(page.getByRole("combobox", { name: "SQL 모드", exact: true })).toBeDisabled();
  const attach = (await commands(page)).filter(r => r.command === "result.attach").length;
  await page.getByRole("button", { name: "행 조회", exact: true }).click();
  await page.getByRole("button", { name: "현재 실행 중지", exact: true }).click();
  const unknown = page.locator(".wb-operation").filter({ has: page.getByRole("heading", { name: "결과 미확인", exact: true }) });
  await expect(unknown).toBeVisible();
  await unknown.getByRole("checkbox", { name: "결과가 미확인 상태임을 이해했습니다.", exact: true }).check();
  await unknown.getByRole("button", { name: "미확인 상태를 인지했음을 기록", exact: true }).click();
  await expect(unknown.getByText("인지 기록이 있습니다. 원래 결과는 계속 미확인입니다.", { exact: true })).toBeVisible();
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(1);
  expect((await commands(page)).filter(r => r.command === "result.attach")).toHaveLength(attach);
  await page.screenshot({ path: info.outputPath("unknown-while-rows.png"), fullPage: true });
});

test("UI-11/12: focused editor controls, themes, fonts and dialogs fit980px at125% without a height acceptance claim", async ({ page }, info) => {
  await page.setViewportSize({ width: 980, height: 820 });
  await connect(page);
  await page.locator(".wb-explorer-fold > summary").click();
  await openSql(page);
  // This explicitly tests the supported focus view, not the exhausted first-open split-height case.
  await page.getByRole("button", { name: "편집기 집중", exact: true }).click();
  await editor(page).fill("SELECT '한글 😀', 42");
  const evidence = [];
  for (const theme of ["dark", "light"] as const) {
    if (await page.locator("html").getAttribute("data-theme") !== theme) await page.getByRole("button", { name: theme === "dark" ? "다크 테마로 전환" : "라이트 테마로 전환", exact: true }).click();
    await page.evaluate(() => { document.documentElement.style.zoom = "1.25"; });
    await expect(page.getByRole("button", { name: "원문 검토", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "원문 검토", exact: true }).click();
    await expect(page.getByRole("region", { name: "네이티브 SQL 검토 결과", exact: true })).toBeVisible();
    const style = await page.evaluate(async () => { await document.fonts.ready; return { theme: document.documentElement.dataset.theme, width: innerWidth, height: innerHeight, dpr: devicePixelRatio, cssZoom: getComputedStyle(document.documentElement).zoom, outerHorizontalOverflow: document.documentElement.scrollWidth > innerWidth, mono: document.fonts.check('400 12px "JetBrains Mono"', "SQL 123"), heading: document.fonts.check('600 16px "Space Grotesk"', "RestorePlane"), loadedKoreanFaces: Array.from(document.fonts).filter(f => /Pretendard/.test(f.family) && f.status === "loaded").length, background: getComputedStyle(document.body).backgroundColor, editorColor: getComputedStyle(document.querySelector(".cm-content")!).color }; });
    expect(style.outerHorizontalOverflow).toBe(false); expect(style.mono).toBe(true); expect(style.heading).toBe(true); expect(style.loadedKoreanFaces).toBeGreaterThan(0);
    expect((await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21aa"]).analyze()).violations).toEqual([]);
    evidence.push(style); await page.screenshot({ path: info.outputPath(`focus980-${theme}-css125.png`), fullPage: true });
  }
  const target = page.getByRole("button", { name: "전체 대상 보기", exact: true });
  await target.click();
  const dialog = page.getByRole("dialog", { name: "실제 연결 대상 전체 보기", exact: true });
  await expect(dialog.locator("pre")).toContainText("workbench.sqlite");
  await page.keyboard.press("Escape"); await expect(dialog).not.toBeVisible(); await expect(target).toBeFocused();
  await writeFile(info.outputPath("theme-font-controls.json"), JSON.stringify({ evidence, firstSplitHeightAcceptance: "NOT RUN:3/3 pending owner decision", physicalBrowserZoom: "NOT RUN: CSS125% separately recorded", physicalIme: "NOT RUN", screenReader: "NOT RUN" }, null, 2));
});

test("UI-12/13: production CSP loads local styles/fonts and defers the editor chunk until explicit SQL navigation", async ({ page }, info) => {
  test.skip(process.env.RESTOREPLANE_UI_QA_STATIC !== "1", "Requires the explicitly built production-policy fixture");
  const paths: string[] = [];
  const origins = new Set<string>();
  page.on("request", request => { const url = new URL(request.url()); paths.push(url.pathname); origins.add(url.origin); });
  await page.addInitScript(() => {
    const violations: { directive: string; blocked: string }[] = [];
    Object.assign(window, { uiPolicyViolations: violations });
    document.addEventListener("securitypolicyviolation", event => violations.push({ directive: event.violatedDirective, blocked: event.blockedURI === "inline" ? "inline" : new URL(event.blockedURI || location.href, location.href).protocol }));
  });
  await connect(page);
  expect(paths.filter(path => /SqlCodeEditor.*\.js$/.test(path))).toHaveLength(0);
  await openSql(page);
  await page.getByRole("button", { name: "편집기 집중", exact: true }).click();
  await editor(page).fill("SELECT '한글', 42");
  expect(paths.filter(path => /SqlCodeEditor.*\.js$/.test(path))).toHaveLength(1);
  await expect(page.locator(".cm-line span").filter({ hasText: "SELECT" })).toBeVisible();
  const policy = await page.locator('meta[http-equiv="Content-Security-Policy"]').getAttribute("content");
  expect(policy).toContain("style-src 'self' 'unsafe-inline'"); expect(policy).toContain("font-src 'self' data:");
  await page.evaluate(async () => document.fonts.ready);
  expect(await page.evaluate(() => (window as unknown as { uiPolicyViolations: object[] }).uiPolicyViolations)).toEqual([]);
  expect(paths.every(path => path.startsWith("/") && !path.includes("node_modules"))).toBe(true);
  expect([...origins]).toEqual(["http://127.0.0.1:1420"]);
  await page.setViewportSize({ width: 980, height: 820 });
  await page.evaluate(() => { document.documentElement.style.zoom = "1.25"; });
  await editor(page).focus(); await editor(page).press("Control+Home");
  await page.screenshot({ path: info.outputPath("offline-focused980-css125-viewport.png"), fullPage: false });
  await writeFile(info.outputPath("offline-policy-evidence.json"), JSON.stringify({ policy, violations: [], origins: [...origins], editorChunkRequests: paths.filter(path => /SqlCodeEditor.*\.js$/.test(path)), totalRequests: paths.length, nativeWebView: "NOT RUN", firstSplitHeightAcceptance: "NOT RUN:owner decision pending", scope: "Production-built explicit mock App using unchanged Tauri CSP; no dev HMR or external styles/fonts" }, null, 2));
});

for (const engine of ["postgresql", "mysql"]) test(`UI-02-analyze: ${engine} keeps original SQL separate from explicitly confirmed Full-mode analysis`, async ({ page }) => {
  await connect(page, `?engine=${engine}`);
  await page.getByRole("combobox", { name: "SQL 모드", exact: true }).selectOption("full");
  await page.getByRole("dialog").getByRole("button", { name: "이 대상의 모드 전환", exact: true }).click();
  await expect(page.getByRole("combobox", { name: "SQL 모드", exact: true })).toHaveValue("full");
  await openSql(page); await editor(page).fill("SELECT 1");
  await page.getByRole("button", { name: "실행 분석 검토", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "실제로 실행하는 분석 SQL을 검토할까요?", exact: true });
  const expected = `${engine === "mysql" ? "EXPLAIN ANALYZE FORMAT=TREE" : "EXPLAIN ANALYZE"} SELECT 1`;
  await expect(dialog.locator("pre")).toHaveText(expected);
  expect((await commands(page)).filter(r => r.command === "query.review")).toHaveLength(0);
  await dialog.getByRole("button", { name: "이 SQL 전체 검토", exact: true }).click();
  await expect.poll(async () => (await commands(page)).find(r => r.command === "query.review")?.payload).toMatchObject({ sql: expected, purpose: "explain_analyze" });
  await expect(editor(page)).toHaveText("SELECT 1");
  expect((await commands(page)).filter(r => r.command === "query.run")).toHaveLength(0);
});

test("UI-12-viewport: fixed header and sidebar positions are captured separately from fullPage stitching", async ({ page }, info) => {
  await page.setViewportSize({ width: 980, height: 820 }); await connect(page);
  await page.locator(".wb-explorer-fold > summary").click(); await openSql(page);
  await page.getByRole("button", { name: "편집기 집중", exact: true }).click();
  await editor(page).fill("SELECT '한글', 42");
  const frames = [];
  const observe = () => page.evaluate(() => {
    const box = (node: Element) => { const r = node.getBoundingClientRect(); return { tag: node.tagName, className: node.className, position: getComputedStyle(node).position, x: r.x, y: r.y, width: r.width, height: r.height }; };
    return { theme: document.documentElement.dataset.theme, scrollX, scrollY, innerWidth, innerHeight, cssZoom: getComputedStyle(document.documentElement).zoom, chrome: Array.from(document.querySelectorAll("header,aside")).slice(0, 10).map(box), transaction: document.querySelector(".wb-transaction") ? box(document.querySelector(".wb-transaction")!) : null };
  });
  for (const theme of ["dark", "light"] as const) {
    if (await page.locator("html").getAttribute("data-theme") !== theme) await page.getByRole("button", { name: theme === "dark" ? "다크 테마로 전환" : "라이트 테마로 전환", exact: true }).click();
    await page.evaluate(() => { document.documentElement.style.zoom = "1.25"; });
    await page.getByRole("button", { name: "원문 검토", exact: true }).click();
    const afterInteraction = await observe();
    await page.screenshot({ path: info.outputPath(`${theme}-after-interaction-viewport.png`), fullPage: false });
    await page.evaluate(() => window.scrollTo(0, 0));
    await expect.poll(() => page.evaluate(() => scrollY)).toBe(0);
    const restoredTop = await observe();
    await page.screenshot({ path: info.outputPath(`${theme}-restored-top-viewport.png`), fullPage: false });
    frames.push({ afterInteraction, restoredTop });
  }
  await writeFile(info.outputPath("viewport-capture-geometry.json"), JSON.stringify({ frames, firstSplitHeightAcceptance: "NOT RUN:3/3 pending owner decision", scope: "Read-only viewport/header/sidebar observations at CSS125%; no product shell change, no new height assertion" }, null, 2));
});


test("DOM-viewport: 200-row SQL keeps bounded cells, trusted wheel and hidden-pane ACK", async ({ page }, info) => {
  await page.setViewportSize({ width: 1240, height: 840 });
  await connect(page, "?wideSql=1"); await openSql(page);
  await page.evaluate(() => { (window as unknown as { workbenchMockControl: { holdWideFrame: boolean } }).workbenchMockControl.holdWideFrame = true; });
  await editor(page).fill("SELECT * FROM bench ORDER BY c7 DESC");
  await page.getByRole("button", { name: "원문 검토", exact: true }).click();
  await page.getByRole("button", { name: "검토한 SQL 실행", exact: true }).click();
  const result = page.locator(".wb-query-results");
  const grid = result.getByRole("grid", { name: "조회 결과", exact: true });
  await expect.poll(async () => (await commands(page)).filter(r => r.command === "result.frames" && r.payload.afterSequence === "2").length).toBe(1);
  await expect.poll(() => grid.getByRole("gridcell").count()).toBeGreaterThan(0);
  const attachBefore = (await commands(page)).filter(r => r.command === "result.attach").length;
  const runBefore = (await commands(page)).filter(r => r.command === "query.run").length;
  expect(attachBefore).toBe(1); expect(runBefore).toBe(1);
  await page.getByRole("button", { name: "행 조회", exact: true }).click();
  await expect(grid).toHaveCount(0);
  await page.evaluate(() => { (window as unknown as { workbenchMockControl: { holdWideFrame: boolean } }).workbenchMockControl.holdWideFrame = false; });
  await expect.poll(async () => (await commands(page)).filter(r => r.command === "result.ack").at(-1)?.payload.highestContiguousSequence).toBe("6");
  await openSql(page);
  await expect(grid).toBeVisible();
  await expect.poll(() => grid.evaluate(node => node.scrollHeight)).toBeGreaterThan(6000);
  await grid.scrollIntoViewIfNeeded(); await grid.focus();
  await grid.evaluate(() => {
    const state: { pending: "keyboard" | "scroll" | null; ready: boolean; samples: { action: string; trusted: boolean; mounted: number; rows: number }[]; settled: Promise<unknown> } = { pending: null, ready: false, samples: [], settled: Promise.resolve(null) };
    let resolveInput: (value: unknown) => void = () => {};
    const receive = (event: Event) => {
      const action = state.pending;
      if (!action || action === "scroll" && event.type !== "wheel" || action === "keyboard" && event.type !== "keydown") return;
      state.pending = null;
      requestAnimationFrame(() => requestAnimationFrame(() => {
        const grid = document.querySelector('[role="grid"]');
        const sample = { action, trusted: event.isTrusted, mounted: document.querySelectorAll('[role="gridcell"]').length, rows: grid?.querySelectorAll('[role="row"]').length ?? 0 };
        state.samples.push(sample); state.ready = true; resolveInput(sample);
      }));
    };
    document.addEventListener("keydown", receive, true); document.addEventListener("wheel", receive, { capture: true, passive: true });
    (window as unknown as { viewportInputProbe: unknown }).viewportInputProbe = {
      arm(action: "keyboard" | "scroll") { if (state.pending) throw Error("already armed"); state.pending = action; state.ready = false; state.settled = new Promise(resolve => { resolveInput = resolve; }); },
      status: () => ({ ready: state.ready, samples: state.samples }), settled: () => state.settled,
    };
  });
  const cdp = await page.context().newCDPSession(page);
  await page.evaluate(() => (window as unknown as { viewportInputProbe: { arm(action: string): void } }).viewportInputProbe.arm("keyboard"));
  await cdp.send("Input.dispatchKeyEvent", { type: "keyDown", key: "ArrowDown", code: "ArrowDown", windowsVirtualKeyCode: 40 });
  await expect.poll(() => page.evaluate(() => (window as unknown as { viewportInputProbe: { status(): { ready: boolean } } }).viewportInputProbe.status().ready)).toBe(true);
  await page.evaluate(() => (window as unknown as { viewportInputProbe: { settled(): Promise<unknown> } }).viewportInputProbe.settled());
  await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", key: "ArrowDown", code: "ArrowDown", windowsVirtualKeyCode: 40 });
  await grid.scrollIntoViewIfNeeded(); await grid.focus();
  const before = await grid.evaluate(node => {
    const box = node.getBoundingClientRect(); const centerX = box.x + box.width / 2, centerY = box.y + box.height / 2;
    const hit = document.elementFromPoint(centerX, centerY);
    let clip = { left: 0, top: 0, right: innerWidth, bottom: innerHeight };
    const scrollports: { className: string; overflowX: string; overflowY: string; x: number; y: number; width: number; height: number }[] = [];
    for (let parent = node.parentElement; parent; parent = parent.parentElement) {
      const style = getComputedStyle(parent), rect = parent.getBoundingClientRect();
      if (/(hidden|auto|scroll|clip)/.test(style.overflowX + " " + style.overflowY)) scrollports.push({ className: parent.className, overflowX: style.overflowX, overflowY: style.overflowY, x: rect.x, y: rect.y, width: rect.width, height: rect.height });
      if (/(hidden|auto|scroll|clip)/.test(style.overflowX)) clip = { ...clip, left: Math.max(clip.left, rect.left), right: Math.min(clip.right, rect.right) };
      if (/(hidden|auto|scroll|clip)/.test(style.overflowY)) clip = { ...clip, top: Math.max(clip.top, rect.top), bottom: Math.min(clip.bottom, rect.bottom) };
    }
    const events: { trusted: boolean; deltaX: number; deltaY: number }[] = [];
    (window as unknown as { wideWheelEvents: typeof events }).wideWheelEvents = events;
    node.addEventListener("wheel", e => { const event = e as WheelEvent; events.push({ trusted: event.isTrusted, deltaX: event.deltaX, deltaY: event.deltaY }); }, { once: true });
    return { clientHeight: node.clientHeight, scrollHeight: node.scrollHeight, top: node.scrollTop, left: node.scrollLeft, mounted: node.querySelectorAll('[role="gridcell"]').length, rows: node.querySelectorAll('[role="row"]').length, columns: node.getAttribute("aria-colcount"), centerX, centerY, clip, scrollports, centerInsideViewport: centerX >= 0 && centerX < innerWidth && centerY >= 0 && centerY < innerHeight, centerInsideScrollports: centerX >= clip.left && centerX < clip.right && centerY >= clip.top && centerY < clip.bottom, centerTargetsGrid: hit === node || !!hit && node.contains(hit), box: { x: box.x, y: box.y, width: box.width, height: box.height } };
  });
  await writeFile(info.outputPath("viewport-before-wheel.json"), JSON.stringify(before, null, 2));
  expect(before.columns).toBe("100"); expect(before.clientHeight).toBeGreaterThan(0); expect(before.clientHeight).toBeLessThanOrEqual(400);
  expect(before.scrollHeight).toBeGreaterThan(6000); expect(before.mounted).toBeGreaterThan(0); expect(before.mounted).toBeLessThanOrEqual(336);
  expect(before.rows).toBeLessThan(201); expect(before.centerInsideViewport).toBe(true); expect(before.centerInsideScrollports).toBe(true); expect(before.centerTargetsGrid).toBe(true);
  await page.evaluate(() => (window as unknown as { viewportInputProbe: { arm(action: string): void } }).viewportInputProbe.arm("scroll"));
  await cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: before.centerX, y: before.centerY, deltaX: 184, deltaY: 120 });
  await expect.poll(() => page.evaluate(() => (window as unknown as { viewportInputProbe: { status(): { ready: boolean } } }).viewportInputProbe.status().ready)).toBe(true);
  await page.evaluate(() => (window as unknown as { viewportInputProbe: { settled(): Promise<unknown> } }).viewportInputProbe.settled());
  const settledInputs = await page.evaluate(() => (window as unknown as { viewportInputProbe: { status(): { samples: { action: string; trusted: boolean; mounted: number; rows: number }[] } } }).viewportInputProbe.status().samples);
  expect(settledInputs.map(input => input.action)).toEqual(["keyboard", "scroll"]);
  expect(settledInputs.every(input => input.trusted && input.mounted <= 336)).toBe(true);
  await expect.poll(() => grid.evaluate(node => node.scrollTop)).toBeGreaterThan(before.top);
  await expect.poll(() => grid.evaluate(node => node.scrollLeft)).toBeGreaterThan(before.left);
  const wheel = await page.evaluate(() => (window as unknown as { wideWheelEvents: { trusted: boolean; deltaX: number; deltaY: number }[] }).wideWheelEvents);
  expect(wheel).toEqual([{ trusted: true, deltaX: 184, deltaY: 120 }]);
  await grid.press("Control+End");
  await expect(grid.getByRole("gridcell", { name: "열_99: 199:99", exact: true })).toHaveAttribute("aria-selected", "true");
  expect(await grid.getByRole("gridcell").count()).toBeLessThanOrEqual(336);
  const selected = await grid.evaluate(node => ({ top: node.scrollTop, left: node.scrollLeft, active: node.getAttribute("aria-activedescendant") }));
  await page.getByRole("button", { name: "행 조회", exact: true }).click(); await openSql(page);
  await expect.poll(() => grid.evaluate(node => ({ top: node.scrollTop, left: node.scrollLeft, active: node.getAttribute("aria-activedescendant") }))).toEqual(selected);
  const requests = await commands(page);
  expect(requests.filter(r => r.command === "result.attach")).toHaveLength(attachBefore);
  expect(requests.filter(r => r.command === "query.run")).toHaveLength(runBefore);
  await writeFile(info.outputPath("wide-sql-viewport-evidence.json"), JSON.stringify({ before, wheel, settledInputs, selected, retainedRow199Reached: true, fixtureRows: 200, fixtureColumns: 100, rowFrames: 4, acceptedThroughSequence: "6", attachCount: attachBefore, queryCount: runBefore, mountedMaximumRequired: 336, viewport: { width: 1240, height: 840 }, scope: "Actual production App/QueryResults/virtualizer with explicit mocked transport; deferred rows continue and ACK while SQL pane is hidden. First keyboard→doubleRAF event-bound settled→keyUp→wheel at full bbox center→event-bound settled uses the original input sequence; no hover or intersection coordinate replacement. Test observer omits native bench_info counters. No native DB, first-open120/100-height acceptance, M1/M2/M3 score, physical IME or full benchmark." }, null, 2));
});

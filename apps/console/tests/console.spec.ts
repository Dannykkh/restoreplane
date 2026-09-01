import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";

const navigationLabels = [
  "Overview",
  "Connections",
  "Databases",
  "Destinations",
  "Backup Plans",
  "Activity",
  "Snapshots",
  "Recovery",
  "Settings",
] as const;

test("fleet dashboard filters, creates and opens an operational scope on desktop and mobile", async ({
  page,
}, testInfo) => {
  await page.goto("/");
  await expect(page).toHaveURL(/#\/workspaces$/);
  const table = page.getByRole("table", { name: "Workspace protection overview" });
  await expect(table).toBeVisible();
  await expect(table.locator("tbody tr")).toHaveCount(3);
  await expect(table.getByText("실행 중", { exact: true })).toBeVisible();
  await expect(table.getByText("Plan 필요", { exact: true })).toBeVisible();
  await expectNoDocumentOverflow(page);
  expect(await wcagViolations(page)).toEqual([]);
  await page.screenshot({
    path: testInfo.outputPath("workspace-fleet.png"),
    fullPage: true,
  });

  await page.getByLabel("Workspace 검색").fill("Analytics");
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await expect(table.getByText("Analytics Lab", { exact: true })).toBeVisible();
  await page.getByLabel("Workspace 검색").fill("");

  const selector = page.getByLabel("Workspace", { exact: true });
  await expect(selector).toHaveValue("00000000-0000-4000-8000-000000000001");
  await expect(selector.getByRole("option", { name: "Default Workspace" })).toBeAttached();

  await page.getByRole("button", { name: "새 Workspace", exact: true }).click();
  await page.getByLabel("Workspace name", { exact: true }).fill("Treasury DR");
  await page.getByRole("button", { name: "Create", exact: true }).click();

  await expect(page).toHaveURL(/#\/overview$/);
  await expect(selector).toHaveValue(/^[0-9a-f-]{36}$/);
  await expect(selector.getByRole("option", { name: "Treasury DR" })).toBeAttached();
  expect(await page.evaluate(() => localStorage.getItem("restoreplane.workspaceId"))).toBe(
    await selector.inputValue(),
  );
  await expect(page.locator(".topbar-context .eyebrow")).toHaveText(
    "Treasury DR",
  );
  expect(await wcagViolations(page)).toEqual([]);
  await page.screenshot({
    path: testInfo.outputPath("workspace-selected.png"),
    fullPage: true,
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByRole("link", { name: "All", exact: true })).toBeVisible();
  await expect(selector).toBeVisible();
  await expectNoDocumentOverflow(page);
});

test("nine-route shell preserves filter and selection through browser history", async ({
  page,
}) => {
  await page.goto(
    "/#/connections?filter=attention&selected=connection-billing-mysql",
  );
  await expect(page.getByRole("heading", { name: "Connections", exact: true })).toBeVisible();

  const navigation = page.getByRole("navigation", { name: "Primary" });
  await expect(navigation.getByRole("link")).toHaveCount(9);
  await expect(navigation.getByRole("link").allTextContents()).resolves.toEqual(
    navigationLabels.map((label) => expect.stringContaining(label)),
  );
  await expect(page.getByLabel("상태")).toHaveValue("attention");
  await expect(
    page.getByRole("complementary", { name: "Billing MySQL details" }),
  ).toBeVisible();

  await navigation.getByRole("link", { name: /Databases/ }).click();
  await expect(page).toHaveURL(/#\/databases$/);
  await expect(page.getByRole("heading", { name: "Databases", exact: true })).toBeVisible();

  await page.goBack();
  await expect(page).toHaveURL(
    /#\/connections\?filter=attention&selected=connection-billing-mysql/,
  );
  await expect(page.getByLabel("상태")).toHaveValue("attention");
  await expect(
    page.getByRole("complementary", { name: "Billing MySQL details" }),
  ).toBeVisible();
});

test("first connection to target selection and plan entry is keyboard-only", async ({
  page,
}) => {
  await page.goto("/#/connections");
  const create = page.getByRole("button", { name: "새 Connection" });
  await create.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("dialog", { name: "Connection onboarding" })).toBeVisible();

  await typeWithKeyboard(page, "연결 이름", "Keyboard PostgreSQL");
  await typeWithKeyboard(page, "호스트", "pg-keyboard.internal");
  await typeWithKeyboard(page, "사용자", "backup_operator");
  await typeWithKeyboard(page, "비밀번호", "temporary-canary-secret");

  await pressButton(page, "Draft 저장");
  await expect(page.getByRole("heading", { name: /연결 가능성과 도구 신뢰/ })).toBeVisible();
  await pressButton(page, "Probe 및 검색");

  await expect(page.getByRole("checkbox", { name: /orders/ })).toBeChecked();
  await expect(page.getByRole("checkbox", { name: /ledger/ })).toBeChecked();
  await pressButton(page, "2개 Target 선택");

  await expect(page.getByText("2개 Database Target을 선택했습니다.")).toBeVisible();
  await expect(page.getByText(/아직 백업이 구성된 것은 아닙니다/)).toBeVisible();
  await pressButton(page, "Backup Plan 시작");

  await expect(page).toHaveURL(
    /#\/backup-plans\?selected=target-connection-preview-\d+-orders&action=new/,
  );
  await expect(page.getByRole("dialog", { name: "Backup Plan workspace" })).toBeVisible();
  await expect(page.getByLabel("Plan 이름")).toBeVisible();
  await expect(
    page.getByRole("checkbox", { name: /orders Keyboard PostgreSQL/ }),
  ).toBeChecked();

  const exposure = await page.evaluate(() => ({
    url: window.location.href,
    storage: Object.values(window.localStorage),
    passwordValue:
      document.querySelector<HTMLInputElement>('input[name="password"]')?.value ?? "",
  }));
  expect(JSON.stringify(exposure)).not.toContain("temporary-canary-secret");
  await expectAllControlsLabeled(page);
});

test("explicit table Scope is discovered, persisted, bound to a Plan, and remains visibly partial", async ({
  page,
}, testInfo) => {
  await page.goto("/#/databases?selected=target-orders");
  await expect(
    page.getByRole("complementary", { name: "orders details" }),
  ).toBeVisible();

  await pressButton(page, "새 선택 범위");
  const scopeDialog = page.getByRole("dialog", { name: "orders Backup Scope" });
  await expect(scopeDialog).toBeVisible();
  await scopeDialog.getByRole("radio", { name: "선택한 테이블만" }).check();
  await scopeDialog.getByLabel("범위 이름").fill("orders 결제 핵심 테이블");
  await scopeDialog.getByRole("button", { name: "테이블 검색" }).click();

  const discovered = scopeDialog.getByRole("group", { name: "발견된 테이블" });
  await expect(discovered).toBeVisible();
  await expect(discovered.getByRole("checkbox", { name: /public\.iwrs/ })).toBeEnabled();
  await discovered.getByRole("checkbox", { name: /public\.iwrs/ }).check();
  await expect(scopeDialog.getByLabel("Backup scope coverage")).toContainText("1 / 3");
  await scopeDialog.getByRole("button", { name: "범위 저장" }).click();

  await expect(page.getByText(/Backup Scope .*저장했습니다/)).toBeVisible();
  const persistedScope = page.getByRole("radio", {
    name: /orders 결제 핵심 테이블.*명시 테이블/,
  });
  await expect(persistedScope).toBeVisible();
  await persistedScope.check();
  await expect(
    persistedScope.locator("..").getByLabel("Backup scope coverage"),
  ).toContainText("1 / 3");
  expect(await wcagViolations(page)).toEqual([]);
  await page.screenshot({
    path: testInfo.outputPath("explicit-table-scope.png"),
    fullPage: true,
  });

  await pressButton(page, "Backup Plan 시작");
  const planDialog = page.getByRole("dialog", { name: "Backup Plan workspace" });
  await expect(planDialog).toBeVisible();
  await planDialog.getByLabel("Plan 이름").fill("orders explicit table protection");
  const wholeScope = planDialog.getByRole("checkbox", { name: /orders 전체 백업/ });
  if (await wholeScope.isChecked()) await wholeScope.uncheck();
  const explicitScope = planDialog.getByRole("checkbox", {
    name: /orders 결제 핵심 테이블/,
  });
  await explicitScope.check();
  await expect(explicitScope).toBeChecked();
  await expect(
    explicitScope.locator("..").getByLabel("Backup scope coverage"),
  ).toContainText("1 / 3");

  await planDialog.getByRole("button", { name: "다음: Destinations" }).click();
  await planDialog.getByRole("checkbox", { name: /Local Recovery Cache/ }).check();
  await planDialog.getByRole("button", { name: "다음: Schedule \/ RPO" }).click();
  await planDialog.getByRole("button", { name: "다음: Retention \/ Verify" }).click();
  await planDialog.getByRole("button", { name: "다음: Preflight" }).click();
  await expect(
    planDialog.getByText("Scopes", { exact: true }).locator(".."),
  ).toContainText("1");
  await expect(
    planDialog.getByRole("button", { name: "검증 후 Plan 게시" }),
  ).toBeEnabled();
  expect(await wcagViolations(page)).toEqual([]);
});

test("Plan preflight blocks SQL Server candidate from the service Engine Matrix", async ({
  page,
}) => {
  await page.goto("/#/backup-plans?action=new");
  const dialog = page.getByRole("dialog", { name: "Backup Plan workspace" });
  await expect(dialog).toBeVisible();

  await dialog.getByLabel("Plan 이름").fill("SQL Server candidate must block");
  await dialog
    .getByRole("checkbox", { name: /finance_warehouse.*Finance SQL Server/ })
    .check();
  await dialog.getByRole("button", { name: "다음: Destinations" }).click();
  await dialog
    .getByRole("checkbox", { name: /Local Recovery Cache/ })
    .check();
  await dialog
    .getByRole("button", { name: "다음: Schedule \/ RPO" })
    .click();
  await dialog
    .getByRole("button", { name: "다음: Retention \/ Verify" })
    .click();
  await dialog.getByRole("button", { name: "다음: Preflight" }).click();

  const matrix = dialog.getByRole("table", {
    name: "Engine capability matrix",
  });
  await expect(matrix.getByText("sqlserver backup")).toBeVisible();
  await expect(matrix.getByText("experimental")).toHaveCount(2);
  await expect(
    dialog.getByRole("heading", {
      name: "요구 capability를 충족하지 못합니다.",
    }),
  ).toBeVisible();
  await expect(
    dialog.getByRole("button", { name: "검증 후 Plan 게시" }),
  ).toBeDisabled();
});

test("manual backup exposes durable progress and fencing-token cancellation", async ({
  page,
}, testInfo) => {
  await page.goto("/#/backup-plans");
  await expect(
    page.getByText("브라우저 검증 데이터", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("매시간 15분", { exact: true })).toBeVisible();

  const runNow = page.getByRole("button", { name: "지금 실행" });
  await expect(runNow).toBeEnabled();
  await runNow.click();

  await expect(
    page.getByRole("heading", { name: "실행 중인 백업" }),
  ).toBeVisible();
  const progress = page.getByRole("progressbar", {
    name: "Production hourly orders 진행률",
  });
  await expect(progress).toHaveAttribute("aria-valuenow", "38");
  await expect(page.getByText("public.orders")).toBeVisible();
  await expect(
    page.getByText(/work-backup-plan-production-hourly-orders-/),
  ).toBeVisible();
  await expectNoDocumentOverflow(page);
  expect(await wcagViolations(page)).toEqual([]);
  await page.screenshot({
    path: testInfo.outputPath("backup-running.png"),
    fullPage: true,
  });

  await page.getByRole("button", { name: "Production hourly orders 취소" }).click();
  await expect(
    page.getByRole("heading", { name: "실행 중인 백업" }),
  ).toBeVisible();
  await expect(page.getByText("1 active / 2 tracked", { exact: true })).toBeVisible();
  await expect(page.getByText("cancelled", { exact: true })).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Production hourly orders 취소" }),
  ).toHaveCount(0);
});

test("local Destination onboarding records writer reader and delete evidence", async ({
  page,
}) => {
  await page.goto("/#/destinations");
  const create = page.getByRole("button", { name: "새 Destination" });
  await create.focus();
  await page.keyboard.press("Enter");
  const dialog = page.getByRole("dialog", {
    name: "Destination onboarding",
  });
  await expect(dialog).toBeVisible();

  await typeWithKeyboard(page, "Destination 이름", "Keyboard Local Vault");
  await typeWithKeyboard(page, "절대 경로", "D:\\RestorePlane\\keyboard-vault");
  await pressButton(page, "역할 자격 증명");
  await expect(
    page.getByRole("heading", {
      name: "Local 경로는 별도 비밀을 저장하지 않습니다.",
    }),
  ).toBeVisible();
  await pressButton(page, "저장 후 역할 Probe");
  await expect(
    page.getByRole("heading", {
      name: /쓰기와 복구 읽기 근거가 확인됐습니다/,
    }),
  ).toBeVisible();
  await expect(dialog.getByText("Role probes passed")).toBeVisible();
  await pressButton(page, "Destination 보기");

  await expect(page).toHaveURL(
    /#\/destinations\?selected=destination-preview-\d+/,
  );
  await expect(
    page.getByRole("complementary", {
      name: "Keyboard Local Vault details",
    }),
  ).toBeVisible();
  await expect(
    page.getByRole("complementary", {
      name: "Keyboard Local Vault details",
    }).getByText("passed"),
  ).toHaveCount(3);
  await expectAllControlsLabeled(page);
  expect(await wcagViolations(page)).toEqual([]);
});

test("100k Activity uses strict keyset pages and a bounded DOM", async ({ page }) => {
  const startedAt = Date.now();
  await page.goto("/#/activity");
  const list = page.getByRole("list", { name: "Activity events" });
  await expect(list).toHaveAttribute("aria-setsize", "100000");
  expect(Date.now() - startedAt).toBeLessThan(2_000);
  await expect(list.locator(":scope > li")).toHaveCount(50);
  expect(await list.locator("*").count()).toBeLessThan(600);
  expect(await page.locator("body *").count()).toBeLessThan(1_500);
  await expect(page.getByText(/100,000 events/)).toBeVisible();

  const previousFirstEvent = await list.locator(":scope > li").first().textContent();
  const pageStartedAt = Date.now();
  await page.getByRole("button", { name: "더 이전 page" }).click();
  await expect(page).toHaveURL(/cursor=/);
  await expect
    .poll(async () => list.locator(":scope > li").first().textContent())
    .not.toBe(previousFirstEvent);
  expect(Date.now() - pageStartedAt).toBeLessThan(1_000);
  await expect(list.locator(":scope > li")).toHaveCount(50);

  await page.goBack();
  await expect(page).toHaveURL(/#\/activity$/);
  await expect
    .poll(async () => list.locator(":scope > li").first().textContent())
    .toBe(previousFirstEvent);
});

test("snapshot-driven create-new recovery uses service preflight and terminal validation", async ({
  page,
}) => {
  await page.goto(
    "/#/snapshots?selected=rp-target-orders-latest",
  );
  await expect(
    page.getByRole("complementary", { name: "orders details" }),
  ).toBeVisible();
  await pressButtonOrLink(page, "안전 복구");
  await expect(
    page.getByRole("heading", { name: "Recovery", exact: true }),
  ).toBeVisible();
  await expect(page.locator('input[name="snapshotId"]')).toHaveCount(0);

  await typeNamedControlWithKeyboard(
    page,
    "restoreTarget",
    "orders_restore_keyboard",
  );
  await pressButton(page, "서비스 preflight 실행");
  await expect(
    page.getByRole("heading", { name: "서비스 preflight", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("region", { name: "Preflight summary" }).getByText("false"),
  ).toBeVisible();
  await pressButton(page, "새 대상 복구 요청");

  await expect(
    page.getByRole("heading", { name: "복구 실행 근거" }),
  ).toBeVisible();
  await expect(page.getByText("restore 뒤 validation이 통과했습니다.")).toBeVisible();
  await expect(page.getByText("cleanup required 근거가 없습니다.")).toBeVisible();
  await expect(page).toHaveURL(/action=work%3Awork-restore-/);

  const evidence = await page.evaluate(() => ({
    url: window.location.href,
    localStorage: Object.values(window.localStorage),
    sessionStorage: Object.values(window.sessionStorage),
    text: document.body.innerText,
  }));
  expect(JSON.stringify(evidence)).not.toContain("preview-memory-token");

  await page.evaluate(() => {
    document.body.style.zoom = "200%";
  });
  await expectNoDocumentOverflow(page);
  expect(await wcagViolations(page)).toEqual([]);
});

test("overwrite confirmation traps and returns focus while keeping the bound token memory-only", async ({
  page,
}) => {
  await page.goto("/#/recovery?selected=rp-target-orders-latest");
  await page
    .getByRole("radio", { name: /기존 Database overwrite/ })
    .check();
  await typeNamedControlWithKeyboard(page, "restoreTarget", "orders");
  await pressButton(page, "서비스 preflight 실행");
  const opener = page.getByRole("button", {
    name: "R-OWNER overwrite 확인",
  });
  await opener.focus();
  await page.keyboard.press("Enter");
  const dialog = page.getByRole("dialog", {
    name: "기존 target overwrite",
  });
  await expect(dialog).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(opener).toBeFocused();

  await page.keyboard.press("Enter");
  await expect(dialog).toBeVisible();
  await page.keyboard.press("Shift+Tab");
  expect(
    await dialog.evaluate((element) =>
      element.contains(document.activeElement),
    ),
  ).toBe(true);
  const confirmationInput = page.getByRole("textbox", {
    name: /계속하려면 target 이름/,
  });
  await confirmationInput.focus();
  await page.keyboard.type("orders");
  await pressButton(page, "1회성 확인 준비");
  await expect(
    page.getByRole("heading", { name: "Service final summary" }),
  ).toBeVisible();
  await pressButton(page, "기존 target overwrite 실행");
  await expect(
    page.getByRole("heading", { name: "복구 실행 근거" }),
  ).toBeVisible();

  const exposure = await page.evaluate(() => ({
    url: window.location.href,
    localStorage: Object.values(window.localStorage),
    sessionStorage: Object.values(window.sessionStorage),
    html: document.body.innerHTML,
  }));
  expect(JSON.stringify(exposure)).not.toContain("preview-memory-token");
  expect(await wcagViolations(page)).toEqual([]);
});

test("restore drill, exact-level verify and catalog preview keep unsupported and unknown states explicit", async ({
  page,
}) => {
  await page.goto("/#/recovery?selected=rp-target-orders-latest");
  await page.getByRole("radio", { name: /L3 restore drill/ }).check();
  await expect(page.getByLabel("Target Connection")).toHaveValue(
    "connection-restore-lab-pg",
  );
  await typeNamedControlWithKeyboard(
    page,
    "restoreTarget",
    "orders_monthly_drill",
  );
  await pressButton(page, "서비스 preflight 실행");
  await expect(
    page.getByRole("region", { name: "Preflight summary" }).getByText(
      "restore_test",
    ),
  ).toBeVisible();
  await pressButton(page, "L3 restore drill 요청");
  await expect(page.getByText("restore 뒤 validation이 통과했습니다.")).toBeVisible();

  await page.goto("/#/recovery?selected=rp-target-orders-latest");
  await pressButton(page, "L3 요청");
  await expect(page.getByText("VERIFICATION_RESTORE_RUN_REQUIRED")).toBeVisible();
  await pressButton(page, "Read-only preview");
  await expect(page.getByText("unknown_key", { exact: true })).toBeVisible();
  await expect(page.getByText("DUPLICATE_RECOVERY_POINT")).toBeVisible();

  await page.goto("/#/snapshots?filter=diagnostics");
  await expect(page.getByText("manifest_unknown_key")).toBeVisible();
  await expect(page.getByRole("link", { name: "안전 복구" })).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "Catalog rebuild preview" }),
  ).toBeVisible();
  expect(await wcagViolations(page)).toEqual([]);
});

test("visual, font, theme, zoom, copy and WCAG matrix", async ({
  page,
}, testInfo) => {
  await page.goto("/#/overview");
  await expect(page.getByRole("heading", { name: /백업 상태를 근거별로/ })).toBeVisible();
  await page.evaluate(() => document.fonts.ready);

  const fonts = await page.evaluate(() => ({
    heading: document.fonts.check('600 16px "Space Grotesk"'),
    mono: document.fonts.check('400 16px "JetBrains Mono"'),
    pretendardLoaded: Array.from(document.fonts).some(
      (face) =>
        face.family.includes("Pretendard") && face.status === "loaded",
    ),
    bodyFamily: getComputedStyle(document.body).fontFamily,
  }));
  expect(fonts.heading).toBe(true);
  expect(fonts.mono).toBe(true);
  expect(fonts.pretendardLoaded).toBe(true);
  expect(fonts.bodyFamily).toContain("Pretendard");
  expect(await wcagViolations(page)).toEqual([]);

  const darkColors = await page.evaluate(() => {
    const style = getComputedStyle(document.body);
    return { background: style.backgroundColor, color: style.color };
  });
  await page.screenshot({
    path: testInfo.outputPath("overview-dark.png"),
    fullPage: true,
  });
  await page.getByRole("button", { name: "라이트 테마로 전환" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  const lightColors = await page.evaluate(() => {
    const style = getComputedStyle(document.body);
    return { background: style.backgroundColor, color: style.color };
  });
  expect(lightColors).not.toEqual(darkColors);
  await page.screenshot({
    path: testInfo.outputPath("overview-light.png"),
    fullPage: true,
  });

  await expectNoDocumentOverflow(page);
  const viewport = page.viewportSize();
  if (!viewport) throw new Error("viewport is required");
  await page.setViewportSize({
    width: Math.max(320, Math.floor(viewport.width / 2)),
    height: viewport.height,
  });
  await expectNoDocumentOverflow(page);
  await expect(page.getByRole("navigation", { name: "Primary" })).toBeVisible();

  await page.setViewportSize(viewport);
  await page.goto("/#/destinations");
  const falseSuccess = await page.locator(".status-badge").evaluateAll((badges) =>
    badges
      .filter((badge) =>
        /configured|unknown|degraded|non_compliant/i.test(
          badge.textContent ?? "",
        ),
      )
      .filter((badge) => badge.getAttribute("data-tone") === "success")
      .map((badge) => badge.textContent),
  );
  expect(falseSuccess).toEqual([]);

  await page.goto("/#/overview");
  expect(await wcagViolations(page)).toEqual([]);

  for (const route of [
    "workspaces",
    "connections",
    "databases",
    "destinations",
    "backup-plans",
    "activity",
    "snapshots",
    "recovery",
    "settings",
  ]) {
    await page.goto(`/#/${route}`);
    await expect(page.getByRole("main")).toBeVisible();
    await expectAllControlsLabeled(page);
  }
});

async function typeWithKeyboard(
  page: Page,
  label: string,
  value: string,
): Promise<void> {
  const control = page.getByLabel(label, { exact: true });
  await control.focus();
  await page.keyboard.type(value);
}

async function typeNamedControlWithKeyboard(
  page: Page,
  name: string,
  value: string,
): Promise<void> {
  const control = page.locator(`[name="${name}"]`);
  await control.focus();
  await page.keyboard.type(value);
}

async function pressButton(page: Page, name: string): Promise<void> {
  const button = page.getByRole("button", { name, exact: true });
  await button.focus();
  await page.keyboard.press("Enter");
}

async function pressButtonOrLink(page: Page, name: string): Promise<void> {
  const control = page.getByRole("button", { name, exact: true }).or(
    page.getByRole("link", { name, exact: true }),
  );
  await control.focus();
  await page.keyboard.press("Enter");
}

async function expectNoDocumentOverflow(page: Page): Promise<void> {
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          document.documentElement.scrollWidth <=
          document.documentElement.clientWidth + 1,
      ),
    )
    .toBe(true);
}

async function expectAllControlsLabeled(page: Page): Promise<void> {
  const unlabeled = await page
    .locator("input:not([type=hidden]), select, textarea")
    .evaluateAll((controls) =>
      controls
        .filter((control) => {
          const field = control as HTMLInputElement;
          return !(
            field.labels?.length ||
            field.getAttribute("aria-label") ||
            field.getAttribute("aria-labelledby") ||
            field.getAttribute("title")
          );
        })
        .map((control) => control.outerHTML),
    );
  expect(unlabeled).toEqual([]);
}

async function wcagViolations(page: Page) {
  const accessibility = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa"])
    .analyze();
  return accessibility.violations.map(({ id, impact, nodes }) => ({
    id,
    impact,
    targets: nodes.map((node) => node.target),
  }));
}

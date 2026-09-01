import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ServiceError } from "../lib/errors";
import type {
  BackupScopeDiscoveryRun,
  BackupScopeObjectPage,
  BackupScopeSaveResult,
  DatabaseTarget,
} from "../lib/types";
import { BackupScopeEditor } from "./BackupScopeEditor";
import { ScopeCoverage } from "./ScopeCoverage";

const target: DatabaseTarget = {
  id: "target-orders",
  connectionId: "connection-pg",
  connectionName: "Production PostgreSQL",
  engine: "postgresql",
  displayName: "orders",
  state: "active",
  missingSinceMs: null,
  sourceFingerprint: "postgresql:orders",
  latestRecoveryPointId: null,
  latestSuccessAtMs: null,
  latestFailureAtMs: null,
  artifactCompleteness: "unknown",
  replicaDurability: "unknown",
  protectionScope: "none",
  protectionCompliance: "not_configured",
  rpoViolation: false,
  verificationState: "not_configured",
  verificationOverdue: false,
  evidenceAtMs: null,
};

const discovery: BackupScopeDiscoveryRun = {
  operationRunId: "operation-discovery-orders",
  workItemId: "work-discovery-orders",
  state: "succeeded",
};

const objectPage: BackupScopeObjectPage = {
  items: [
    {
      engineObjectId: "opaque:pg:4711/iwrs",
      schemaName: "public",
      objectName: "iwrs",
      objectType: "table",
      eligibility: "eligible",
      exclusionReason: null,
      fingerprint: "fingerprint-iwrs",
      discoveryRunId: discovery.operationRunId,
    },
    {
      engineObjectId: "opaque:pg:4712/audit_log",
      schemaName: "public",
      objectName: "audit_log",
      objectType: "table",
      eligibility: "eligible",
      exclusionReason: null,
      fingerprint: "fingerprint-audit",
      discoveryRunId: discovery.operationRunId,
    },
    {
      engineObjectId: "opaque:pg:system/catalog",
      schemaName: "pg_catalog",
      objectName: "pg_statistic",
      objectType: "table",
      eligibility: "excluded",
      exclusionReason: "system table",
      fingerprint: "fingerprint-system",
      discoveryRunId: discovery.operationRunId,
    },
  ],
  nextCursor: null,
  totalCount: 3,
  eligibleCount: 2,
  discoveryRunId: discovery.operationRunId,
  inventoryHashBlake3: "a".repeat(64),
  observedAtMs: Date.now(),
};

describe("BackupScopeEditor", () => {
  it("requires fresh discovery and an eligible selection for explicit-table scopes", async () => {
    const user = userEvent.setup();
    const runDiscovery = vi.fn().mockResolvedValue(discovery);
    const loadObjects = vi.fn().mockResolvedValue(objectPage);
    const saveScope = vi.fn().mockResolvedValue({
      scopeId: "scope-orders-iwrs",
      databaseTargetId: target.id,
      mode: "explicit_tables",
      state: "active",
      selectedObjectCount: 1,
      selectorHashBlake3: "b".repeat(64),
      sourceFingerprint: target.sourceFingerprint,
    } satisfies BackupScopeSaveResult);

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={runDiscovery}
        loadObjects={loadObjects}
        saveScope={saveScope}
        onSaved={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("radio", { name: "선택한 테이블만" }));
    expect(screen.getByRole("button", { name: "범위 저장" })).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "테이블 검색" }));
    expect(await screen.findByText("public.iwrs")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "범위 저장" })).toBeDisabled();

    await user.click(screen.getByRole("checkbox", { name: /public\.iwrs/ }));
    expect(screen.getByText("1 / 2")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "범위 저장" })).toBeEnabled();
    await user.click(screen.getByRole("button", { name: "범위 저장" }));

    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(1));
    expect(saveScope).toHaveBeenCalledWith(
      {
        databaseTargetId: "target-orders",
        displayName: "orders 선택 백업",
        mode: "explicit_tables",
        discoveryRunId: "operation-discovery-orders",
        engineObjectIds: ["opaque:pg:4711/iwrs"],
      },
      expect.any(String),
    );
  });

  it("keeps server filters on the cursor and preserves selections across filtered pages", async () => {
    const user = userEvent.setup();
    const secondAuditObject = {
      ...objectPage.items[1]!,
      engineObjectId: "opaque:pg:4713/audit_archive",
      objectName: "audit_archive",
      fingerprint: "fingerprint-audit-archive",
    };
    const loadObjects = vi.fn().mockImplementation(async (request) => {
      if (request.cursor === "filtered-next") {
        return {
          ...objectPage,
          items: [secondAuditObject],
          nextCursor: null,
          totalCount: 2,
        };
      }
      if (
        request.search === "audit" &&
        request.schemaName === "public" &&
        request.eligibility === "eligible"
      ) {
        return {
          ...objectPage,
          items: [objectPage.items[1]],
          nextCursor: "filtered-next",
          totalCount: 2,
        };
      }
      return {
        ...objectPage,
        items: [objectPage.items[0]],
        nextCursor: "inventory-next",
      };
    });
    const saveScope = vi.fn().mockResolvedValue({
      scopeId: "scope-orders-filtered",
      databaseTargetId: target.id,
      mode: "explicit_tables",
      state: "active",
      selectedObjectCount: 2,
      selectorHashBlake3: "d".repeat(64),
      sourceFingerprint: target.sourceFingerprint,
    } satisfies BackupScopeSaveResult);

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={vi.fn().mockResolvedValue(discovery)}
        loadObjects={loadObjects}
        saveScope={saveScope}
        onSaved={vi.fn()}
      />,
    );

    await user.click(document.querySelector<HTMLInputElement>('input[value="explicit_tables"]')!);
    await user.click(screen.getByRole("button", { name: "테이블 검색" }));
    const firstSelection = await screen.findByRole("checkbox", { name: /public\.iwrs/ });
    await user.click(firstSelection);

    await user.type(screen.getByRole("searchbox"), "audit");
    await user.type(screen.getByRole("textbox", { name: "Schema" }), "public");
    await user.selectOptions(screen.getByRole("combobox", { name: "Eligibility" }), "eligible");
    const auditSelection = await screen.findByRole("checkbox", { name: /public\.audit_log/ });
    await user.click(auditSelection);
    expect(screen.getByTestId("object-filter-total")).toHaveTextContent("Filtered total: 2");

    await user.click(screen.getByRole("button", { name: "다음 페이지" }));
    expect(await screen.findByText("public.audit_archive")).toBeInTheDocument();
    expect(loadObjects).toHaveBeenCalledWith({
      databaseTargetId: target.id,
      discoveryRunId: discovery.operationRunId,
      cursor: "filtered-next",
      limit: 100,
      search: "audit",
      schemaName: "public",
      eligibility: "eligible",
    });

    const submit = document.querySelector<HTMLButtonElement>('button[type="submit"]')!;
    await user.click(submit);
    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(1));
    expect(saveScope.mock.calls[0]?.[0].engineObjectIds).toEqual(expect.arrayContaining([
      objectPage.items[0]!.engineObjectId,
      objectPage.items[1]!.engineObjectId,
    ]));
  });

  it("ignores a stale filtered response that completes after a newer search", async () => {
    const user = userEvent.setup();
    let resolveStale: ((page: BackupScopeObjectPage) => void) | undefined;
    const stalePage = new Promise<BackupScopeObjectPage>((resolve) => {
      resolveStale = resolve;
    });
    const auditPage = {
      ...objectPage,
      items: [objectPage.items[1]],
      totalCount: 1,
    };
    const loadObjects = vi.fn().mockImplementation((request) => {
      if (request.search === "iwrs") return stalePage;
      if (request.search === "audit") return Promise.resolve(auditPage);
      return Promise.resolve(objectPage);
    });

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={vi.fn().mockResolvedValue(discovery)}
        loadObjects={loadObjects}
        saveScope={vi.fn()}
        onSaved={vi.fn()}
      />,
    );

    await user.click(document.querySelector<HTMLInputElement>('input[value="explicit_tables"]')!);
    await user.click(screen.getByRole("button", { name: "테이블 검색" }));
    await screen.findByText("public.iwrs");
    const search = screen.getByRole("searchbox");
    await user.type(search, "iwrs");
    await waitFor(() => expect(loadObjects).toHaveBeenCalledWith(expect.objectContaining({
      cursor: null,
      search: "iwrs",
    })));
    await user.clear(search);
    await user.type(search, "audit");
    await waitFor(() => expect(loadObjects).toHaveBeenCalledWith(expect.objectContaining({
      cursor: null,
      search: "audit",
    })));
    await waitFor(() => expect(screen.getByTestId("object-filter-total")).toHaveTextContent(
      "Filtered total: 1",
    ));

    await act(async () => {
      resolveStale?.({
        ...objectPage,
        items: [objectPage.items[0]],
        totalCount: 1,
      });
    });
    expect(screen.queryByText("public.iwrs")).not.toBeInTheDocument();
    expect(screen.getByText("public.audit_log")).toBeInTheDocument();
  });

  it("uses the filtered server total as the empty-result truth", async () => {
    const user = userEvent.setup();
    const loadObjects = vi.fn().mockImplementation(async (request) =>
      request.search === "nothing"
        ? { ...objectPage, items: [], nextCursor: null, totalCount: 0 }
        : objectPage
    );

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={vi.fn().mockResolvedValue(discovery)}
        loadObjects={loadObjects}
        saveScope={vi.fn()}
        onSaved={vi.fn()}
      />,
    );

    await user.click(document.querySelector<HTMLInputElement>('input[value="explicit_tables"]')!);
    await user.click(screen.getByRole("button", { name: "테이블 검색" }));
    await screen.findByText("public.iwrs");
    await user.type(screen.getByRole("searchbox"), "nothing");

    await waitFor(() => expect(screen.getByTestId("object-filter-total")).toHaveTextContent(
      "Filtered total: 0",
    ));
    expect(screen.getByRole("status")).toHaveTextContent(
      "No objects match the server-side filters.",
    );
    expect(loadObjects).toHaveBeenCalledWith({
      databaseTargetId: target.id,
      discoveryRunId: discovery.operationRunId,
      cursor: null,
      limit: 100,
      search: "nothing",
      schemaName: null,
      eligibility: null,
    });
  });

  it("saves a whole-database scope without running discovery", async () => {
    const user = userEvent.setup();
    const runDiscovery = vi.fn();
    const saveScope = vi.fn().mockResolvedValue({
      scopeId: "target-orders",
      databaseTargetId: target.id,
      mode: "whole_database",
      state: "active",
      selectedObjectCount: null,
      selectorHashBlake3: "c".repeat(64),
      sourceFingerprint: target.sourceFingerprint,
    } satisfies BackupScopeSaveResult);

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={runDiscovery}
        loadObjects={vi.fn()}
        saveScope={saveScope}
        onSaved={vi.fn()}
      />,
    );

    expect(screen.getByText("현재 및 이후 생성되는 테이블 포함")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "범위 저장" }));

    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(1));
    expect(runDiscovery).not.toHaveBeenCalled();
    expect(saveScope).toHaveBeenCalledWith(
      {
        databaseTargetId: "target-orders",
        displayName: "orders 전체 백업",
        mode: "whole_database",
        engineObjectIds: [],
      },
      expect.any(String),
    );
  });

  it("waits for a queued durable discovery work item before reading objects", async () => {
    const user = userEvent.setup();
    const queuedDiscovery: BackupScopeDiscoveryRun = {
      ...discovery,
      state: "queued",
    };
    const readWorkItem = vi.fn().mockResolvedValue({ state: "succeeded" });
    const loadObjects = vi.fn().mockResolvedValue(objectPage);

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={vi.fn().mockResolvedValue(queuedDiscovery)}
        readWorkItem={readWorkItem}
        loadObjects={loadObjects}
        saveScope={vi.fn()}
        onSaved={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("radio", { name: "선택한 테이블만" }));
    await user.click(screen.getByRole("button", { name: "테이블 검색" }));
    expect(await screen.findByText("public.iwrs")).toBeInTheDocument();

    expect(readWorkItem).toHaveBeenCalledWith("work-discovery-orders");
    expect(readWorkItem.mock.invocationCallOrder[0]).toBeLessThan(
      loadObjects.mock.invocationCallOrder[0]!,
    );
  });

  it("reuses a discovery attempt key only after an ambiguous transport error", async () => {
    const user = userEvent.setup();
    const runDiscovery = vi.fn()
      .mockRejectedValueOnce(new ServiceError("IPC_PIPE_CONNECT_FAILED"))
      .mockResolvedValue(discovery);
    const loadObjects = vi.fn().mockResolvedValue(objectPage);

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={runDiscovery}
        loadObjects={loadObjects}
        saveScope={vi.fn()}
        onSaved={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("radio", { name: "선택한 테이블만" }));
    const discoverButton = screen.getByRole("button", { name: "테이블 검색" });
    await user.click(discoverButton);
    await waitFor(() => expect(runDiscovery).toHaveBeenCalledTimes(1));
    await user.click(discoverButton);
    expect(await screen.findByText("public.iwrs")).toBeInTheDocument();
    await user.click(discoverButton);
    await waitFor(() => expect(runDiscovery).toHaveBeenCalledTimes(3));

    const firstKey = runDiscovery.mock.calls[0]?.[1];
    const retryKey = runDiscovery.mock.calls[1]?.[1];
    const nextSuccessKey = runDiscovery.mock.calls[2]?.[1];
    expect(firstKey).toEqual(expect.any(String));
    expect(retryKey).toBe(firstKey);
    expect(nextSuccessKey).not.toBe(retryKey);
  });

  it("reuses a save key after transport ambiguity and clears it after authenticated outcomes", async () => {
    const user = userEvent.setup();
    const saved = {
      scopeId: "target-orders",
      databaseTargetId: target.id,
      mode: "whole_database",
      state: "active",
      selectedObjectCount: null,
      selectorHashBlake3: "c".repeat(64),
      sourceFingerprint: target.sourceFingerprint,
    } satisfies BackupScopeSaveResult;
    const saveScope = vi.fn()
      .mockRejectedValueOnce(new ServiceError("IPC_PIPE_CONNECT_FAILED"))
      .mockRejectedValueOnce(new ServiceError("APPLICATION_FORBIDDEN", {
        requestId: "request-authenticated",
      }))
      .mockResolvedValue(saved);

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={vi.fn()}
        loadObjects={vi.fn()}
        saveScope={saveScope}
        onSaved={vi.fn()}
      />,
    );

    const saveButton = screen.getByRole("button", { name: "범위 저장" });
    await user.click(saveButton);
    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(1));
    await user.click(saveButton);
    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(2));
    await user.click(saveButton);
    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(3));
    await user.click(saveButton);
    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(4));

    const transportKey = saveScope.mock.calls[0]?.[1];
    const ambiguousRetryKey = saveScope.mock.calls[1]?.[1];
    const authenticatedRetryKey = saveScope.mock.calls[2]?.[1];
    const postSuccessKey = saveScope.mock.calls[3]?.[1];
    expect(transportKey).toEqual(expect.any(String));
    expect(ambiguousRetryKey).toBe(transportKey);
    expect(authenticatedRetryKey).not.toBe(ambiguousRetryKey);
    expect(postSuccessKey).not.toBe(authenticatedRetryKey);
  });

  it("locks a scope save after an outcome-unknown response and directs the operator to Activity", async () => {
    const user = userEvent.setup();
    const saveScope = vi.fn().mockRejectedValue(new ServiceError(
      "APPLICATION_OUTCOME_UNKNOWN",
      { requestId: "request-outcome-unknown" },
    ));

    render(
      <BackupScopeEditor
        target={target}
        runDiscovery={vi.fn()}
        loadObjects={vi.fn()}
        saveScope={saveScope}
        onSaved={vi.fn()}
      />,
    );

    const saveButton = screen.getByRole("button", { name: "범위 저장" });
    await user.click(saveButton);
    expect(await screen.findByRole("alert")).toHaveTextContent("APPLICATION_OUTCOME_UNKNOWN");
    expect(screen.getByRole("alert")).toHaveTextContent("Activity");

    await user.click(saveButton);
    await waitFor(() => expect(saveScope).toHaveBeenCalledTimes(1));
  });
});

describe("ScopeCoverage", () => {
  it("distinguishes intentional partial coverage from new and missing table drift", () => {
    render(
      <ScopeCoverage
        mode="explicit_tables"
        selectedObjectCount={4}
        eligibleObjectCount={12}
        newObjectCount={2}
        missingObjectCount={1}
      />,
    );

    expect(screen.getByText("4 / 12")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("새 테이블 2개 미보호");
    expect(screen.getByRole("alert")).toHaveTextContent("선택 테이블 1개 누락");
  });
});

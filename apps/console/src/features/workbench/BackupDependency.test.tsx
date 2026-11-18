import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { BackupScope, DatabaseTarget, Snapshot } from "../../lib/types";
import { WorkbenchClient } from "../../lib/workbench";
import type { WorkbenchProtectionGateway } from "../../lib/workbenchProtection";
import { nativeU64, type Session } from "../../lib/workbenchTypes";
import { ProtectionPanel } from "./ProtectionPanel";

const session: Session = { workspaceId: "workspace", connectionProfileId: "profile", databaseTargetId: "target", sessionId: "session", connectionGeneration: nativeU64("1"), descriptorRevision: "a".repeat(64), executionEpoch: nativeU64("1"), state: "ready", mode: "read_write", actualContext: { endpoint: "fixture.invalid:5432", database: "ledger", role: "writer", purpose: "source" }, capabilities: {} };
const scope = { id: "scope-a", displayName: "선택 원장 범위", databaseTargetId: "target", connectionId: "profile", engine: "postgresql", sourceFingerprint: "b".repeat(64), selectorHashBlake3: "c".repeat(64), sourceDrift: false, state: "active", latestRecoveryPointId: "recovery-a", latestBackupState: "succeeded" } as BackupScope;
const snapshot = { id: "recovery-a", sourceConnectionId: "profile", sourceTargetIdentity: "b".repeat(64), engine: "postgresql", state: "complete", artifactCompleteness: "complete", signatureStatus: "valid", requiredPartCount: 2, presentPartCount: 2, committedAtMs: Date.now() - 600000 } as Snapshot;
function mount(scopes: BackupScope[] = [scope], point: Snapshot = snapshot) {
  const invoke = vi.fn(async () => { throw new Error("No SQL or workbench request expected"); });
  const gateway = { scopes: vi.fn(async () => ({ items: scopes, nextCursor: null })), snapshot: vi.fn(async () => point), work: vi.fn(), discover: vi.fn(), save: vi.fn() } satisfies WorkbenchProtectionGateway;
  render(<ProtectionPanel client={new WorkbenchClient({ available: true, invoke })} gateway={gateway} session={session} target={{ displayName: "원장", engine: "postgresql" } as DatabaseTarget} currentObject={null} />);
  fireEvent.click(screen.getByRole("checkbox", { name: "변경 전에 백업을 별도 작업으로 준비" }));
  return { gateway, invoke };
}
async function choose(): Promise<void> {
  await screen.findByRole("option", { name: /선택 원장 범위/ });
  fireEvent.change(screen.getByRole("combobox", { name: "참고할 백업 범위" }), { target: { value: "scope-a" } });
  fireEvent.click(screen.getByRole("button", { name: "복구지점 상세 확인 후 근거 고정" }));
}
describe("explicit exact-scope recovery-point dependency with injected service gateway", () => {
  it("pins a verified exact recovery point, keeps bounded service queries, and never sends SQL after duplicate completion/failure hints", async () => {
    const { gateway, invoke } = mount(); await choose();
    expect(await screen.findByText(/참고 근거 고정 · 대상 target · 범위 선택 원장 범위/)).toBeVisible();
    expect(gateway.scopes).toHaveBeenCalledWith("target", null); expect(gateway.snapshot).toHaveBeenCalledExactlyOnceWith("recovery-a");
    expect(screen.getByRole("link", { name: "고정한 복구지점 근거 열기" })).toHaveAttribute("href", "#/snapshots?selected=recovery-a&target=target&scope=scope-a");
    for (const state of ["succeeded", "succeeded", "failed"]) window.dispatchEvent(new CustomEvent("service:activity", { detail: { workItemId: "backup-work", state } }));
    fireEvent.click(screen.getByRole("button", { name: "범위 처음부터 새로 확인" }));
    await vi.waitFor(() => expect(gateway.scopes).toHaveBeenCalledTimes(2));
    expect(screen.queryByText(/참고 근거 고정 · 대상/)).not.toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalled(); expect(gateway.work).not.toHaveBeenCalled(); expect(gateway.discover).not.toHaveBeenCalled(); expect(gateway.save).not.toHaveBeenCalled();
  });
  it.each([
    { state: "incomplete" }, { artifactCompleteness: "incomplete" }, { signatureStatus: "invalid" },
    { sourceTargetIdentity: "d".repeat(64) }, { sourceConnectionId: "other-profile" }, { id: "other-point" }, { presentPartCount: 1 },
  ])("refuses invalid, partial, or retargeted detail %j without auto-dispatch", async (change) => {
    const { invoke } = mount([scope], { ...snapshot, ...change } as Snapshot); await choose();
    expect(await screen.findByRole("alert")).toBeVisible(); expect(screen.queryByText(/참고 근거 고정 · 대상/)).not.toBeInTheDocument(); expect(invoke).not.toHaveBeenCalled();
  });
  it("refuses a wrong-target scope page before loading any point", async () => {
    const { invoke, gateway } = mount([{ ...scope, databaseTargetId: "other-target" }]);
    expect(await screen.findByRole("alert")).toBeVisible(); expect(gateway.snapshot).not.toHaveBeenCalled(); expect(invoke).not.toHaveBeenCalled();
  });
});

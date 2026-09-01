import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { BackupPlans } from "./BackupPlans";
import { Destinations } from "./Destinations";
import { Overview } from "./Overview";

const apiMocks = vi.hoisted(() => ({
  listConnections: vi.fn(),
  listDatabaseTargets: vi.fn(),
  listDestinations: vi.fn(),
  listPlans: vi.fn(),
  listActivity: vi.fn(),
  probeDestination: vi.fn(),
}));

vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  ...apiMocks,
}));

const emptyPage = { items: [], nextCursor: null, totalCount: 0 };
const destination = {
  id: "destination-1",
  displayName: "Recovery Vault",
  kind: "local",
  endpoint: "D:\\Backups",
  state: "ready",
  immutabilityCapability: "none",
  lastProbeStatus: "passed",
  lastProbeAtMs: 1,
  updatedAtMs: 1,
  roles: [
    {
      role: "backup_writer",
      credentialBound: true,
      probeStatus: "passed",
      probedAtMs: 1,
    },
    {
      role: "restore_reader",
      credentialBound: true,
      probeStatus: "passed",
      probedAtMs: 1,
    },
  ],
};

describe("cached inventory mutation gates", () => {
  beforeEach(() => {
    for (const mock of Object.values(apiMocks)) mock.mockReset();
    apiMocks.listConnections.mockResolvedValue(emptyPage);
    apiMocks.listDatabaseTargets.mockResolvedValue(emptyPage);
    apiMocks.listDestinations.mockResolvedValue({
      items: [destination],
      nextCursor: null,
      totalCount: 1,
    });
    apiMocks.listPlans.mockResolvedValue(emptyPage);
    apiMocks.listActivity.mockResolvedValue(emptyPage);
    apiMocks.probeDestination.mockResolvedValue({
      workItemId: "work-probe-1",
      state: "succeeded",
    });
  });

  it("disables plan changes after a refresh fails with cached complete inventories", async () => {
    const user = userEvent.setup();
    apiMocks.listPlans
      .mockResolvedValueOnce(emptyPage)
      .mockRejectedValueOnce(new Error("plan refresh failed"));

    render(<BackupPlans route={{ page: "backup-plans" }} />);

    const create = await screen.findByRole("button", { name: "새 Backup Plan" });
    await waitFor(() => expect(create).toBeEnabled());
    await user.click(screen.getByRole("button", { name: "Plan 근거 새로고침" }));

    expect(await screen.findByText("마지막 Plan 목록입니다.")).toBeInTheDocument();
    expect(create).toBeDisabled();
  });

  it("disables destination creation and probe after refresh failure retains cached rows", async () => {
    const user = userEvent.setup();
    apiMocks.listDestinations
      .mockResolvedValueOnce({
        items: [destination],
        nextCursor: null,
        totalCount: 1,
      })
      .mockRejectedValueOnce(new Error("destination refresh failed"));

    render(<Destinations route={{ page: "destinations" }} />);

    const probe = await screen.findByRole("button", { name: "역할별 Probe" });
    await waitFor(() => expect(probe).toBeEnabled());
    await user.click(probe);

    expect(await screen.findByText("마지막 Destination 근거입니다.")).toBeInTheDocument();
    expect(probe).toBeDisabled();
    expect(screen.getByRole("button", { name: "새 Destination" })).toBeDisabled();
  });

  it("labels cached Overview evidence unknown instead of claiming no intervention", async () => {
    const user = userEvent.setup();
    apiMocks.listConnections
      .mockResolvedValueOnce(emptyPage)
      .mockRejectedValueOnce(new Error("overview refresh failed"));

    render(<Overview />);

    const summary = await screen.findByRole("region", { name: "Protection summary" });
    expect(within(summary).getByText("현재 조회 근거에서 개입이 필요한 Target이 없습니다."))
      .toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "근거 새로고침" }));

    expect(await screen.findByText("마지막 Overview 근거입니다.")).toBeInTheDocument();
    expect(within(summary).queryByText("현재 조회 근거에서 개입이 필요한 Target이 없습니다."))
      .not.toBeInTheDocument();
    expect(within(summary).getByText(/개입 필요 여부를 알 수 없습니다/))
      .toBeInTheDocument();
  });
});

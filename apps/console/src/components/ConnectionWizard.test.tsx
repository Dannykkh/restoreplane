import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ServiceError } from "../lib/errors";
import { ConnectionWizard } from "./ConnectionWizard";

const apiMocks = vi.hoisted(() => ({
  saveConnection: vi.fn(),
  probeConnection: vi.fn(),
  discoverConnection: vi.fn(),
  listDiscoveryResults: vi.fn(),
  selectConnectionTargets: vi.fn(),
  getWorkItem: vi.fn(),
}));

vi.mock("../lib/api", () => apiMocks);

describe("ConnectionWizard mutation attempts", () => {
  beforeEach(() => {
    Object.values(apiMocks).forEach((mock) => mock.mockReset());
  });

  it("keeps a completed probe and retries only discovery with its ambiguous key", async () => {
    const user = userEvent.setup();
    apiMocks.saveConnection.mockResolvedValue({ connectionId: "connection-primary" });
    apiMocks.probeConnection.mockResolvedValue({
      workItemId: "work-probe",
      state: "succeeded",
    });
    apiMocks.discoverConnection
      .mockRejectedValueOnce(new ServiceError("IPC_PIPE_CONNECT_FAILED"))
      .mockResolvedValue({
        workItemId: "work-discovery",
        state: "succeeded",
        discoveryRunId: "discovery-primary",
        discovered: [{
          engineObjectId: "opaque:postgresql:orders",
          databaseName: "orders",
          eligibility: "eligible",
          exclusionReason: null,
        }],
      });
    apiMocks.listDiscoveryResults.mockResolvedValue({
      items: [{
        engineObjectId: "opaque:postgresql:orders",
        databaseName: "orders",
        eligibility: "eligible",
        exclusionReason: null,
      }],
      nextCursor: null,
      totalCount: 1,
    });

    render(
      <ConnectionWizard onCancel={vi.fn()} onComplete={vi.fn()} />,
    );

    await user.type(screen.getByRole("textbox", { name: "연결 이름" }), "Primary");
    await user.type(screen.getByRole("textbox", { name: "호스트" }), "db.internal");
    await user.type(screen.getByRole("textbox", { name: "사용자" }), "operator");
    await user.type(screen.getByLabelText("비밀번호"), "secret");
    await user.click(screen.getByRole("button", { name: "Draft 저장" }));

    const probeAndDiscover = await screen.findByRole("button", { name: "Probe 및 검색" });
    await user.click(probeAndDiscover);
    await waitFor(() => expect(apiMocks.discoverConnection).toHaveBeenCalledTimes(1));
    expect(screen.getByRole("button", { name: "Probe 및 검색" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Probe 및 검색" }));
    expect(await screen.findByRole("checkbox", { name: /orders/ })).toBeChecked();

    expect(apiMocks.probeConnection).toHaveBeenCalledTimes(1);
    expect(apiMocks.discoverConnection).toHaveBeenCalledTimes(2);
    expect(apiMocks.discoverConnection.mock.calls[1]?.[1]).toBe(
      apiMocks.discoverConnection.mock.calls[0]?.[1],
    );
  });

  it("fails closed when the discovery cursor cannot prove a complete inventory", async () => {
    const user = userEvent.setup();
    apiMocks.saveConnection.mockResolvedValue({ connectionId: "connection-primary" });
    apiMocks.probeConnection.mockResolvedValue({
      workItemId: "work-probe",
      state: "succeeded",
    });
    apiMocks.discoverConnection.mockResolvedValue({
      workItemId: "work-discovery",
      state: "succeeded",
      discoveryRunId: "discovery-primary",
      discovered: [],
    });
    apiMocks.listDiscoveryResults.mockResolvedValue({
      items: [{
        engineObjectId: "opaque:postgresql:orders",
        databaseName: "orders",
        eligibility: "eligible",
        exclusionReason: null,
      }],
      nextCursor: "stuck-cursor",
      totalCount: 2,
    });

    render(<ConnectionWizard onCancel={vi.fn()} onComplete={vi.fn()} />);

    await user.type(screen.getByRole("textbox", { name: "연결 이름" }), "Primary");
    await user.type(screen.getByRole("textbox", { name: "호스트" }), "db.internal");
    await user.type(screen.getByRole("textbox", { name: "사용자" }), "operator");
    await user.type(screen.getByLabelText("비밀번호"), "secret");
    await user.click(screen.getByRole("button", { name: "Draft 저장" }));
    await user.click(await screen.findByRole("button", { name: "Probe 및 검색" }));

    expect(await screen.findByText("Discovery inventory is incomplete.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Target 선택/ })).toBeDisabled();
    expect(apiMocks.listDiscoveryResults).toHaveBeenCalledTimes(2);
    expect(apiMocks.selectConnectionTargets).not.toHaveBeenCalled();
  });
});

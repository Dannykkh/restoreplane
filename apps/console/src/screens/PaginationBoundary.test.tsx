import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Databases } from "./Databases";
import { Overview } from "./Overview";

const pageMocks = vi.hoisted(() => ({
  listConnections: vi.fn(),
  listDatabaseTargets: vi.fn(),
  listBackupScopes: vi.fn(),
}));

vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  listConnections: pageMocks.listConnections,
  listDatabaseTargets: pageMocks.listDatabaseTargets,
  listBackupScopes: pageMocks.listBackupScopes,
}));

describe("bounded inventory pagination", () => {
  beforeEach(() => {
    pageMocks.listConnections.mockReset().mockImplementation(async () => ({
      items: [],
      nextCursor: null,
      totalCount: 0,
    }));
    pageMocks.listDatabaseTargets.mockReset().mockImplementation(async () => ({
      items: [],
      nextCursor: "stuck-cursor",
      totalCount: 1,
    }));
    pageMocks.listBackupScopes.mockReset().mockResolvedValue({
      items: [],
      nextCursor: null,
    });
  });

  it("labels Overview counts as observed and partial when a cursor does not terminate", async () => {
    render(<Overview />);

    expect(await screen.findByText("Protection inventory is partial.")).toBeInTheDocument();
    expect(screen.getByText("Observed protected targets")).toBeInTheDocument();
    expect(pageMocks.listDatabaseTargets).toHaveBeenCalledTimes(2);
  });

  it("marks Databases filters and actions partial when a cursor does not terminate", async () => {
    render(<Databases route={{ page: "databases" }} />);

    expect(await screen.findByText("Database inventory is partial.")).toBeInTheDocument();
    expect(screen.getByText(/partial inventory/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "선택 DB로 Plan 만들기" })).toBeDisabled();
    expect(pageMocks.listDatabaseTargets).toHaveBeenCalledTimes(2);
  });
});

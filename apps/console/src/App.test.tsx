import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { App } from "./App";

describe("application shell", () => {
  it("starts with a fleet dashboard and enters a selected workspace", async () => {
    const user = userEvent.setup();
    window.location.hash = "";
    render(<App />);

    expect(
      await screen.findByRole("heading", { name: "모든 백업 경계를 한눈에 봅니다" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("table", { name: "Workspace protection overview" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /Default Workspace 열기/ }));

    await waitFor(() => expect(window.location.hash).toBe("#/overview"));
    expect(await screen.findByText("백업 상태를 근거별로 봅니다")).toBeInTheDocument();
  });

  it("renders all navigation landmarks and identifies browser evidence as a fixture", async () => {
    window.location.hash = "#/overview";
    render(<App />);
    await waitFor(() => expect(screen.getByRole("main")).toBeInTheDocument());

    const navigation = screen.getByRole("navigation", { name: "Primary" });
    expect(navigation).toBeInTheDocument();
    expect(
      await screen.findByRole("combobox", { name: "Workspace" }),
    ).toHaveValue("00000000-0000-4000-8000-000000000001");
    expect(screen.getByRole("option", { name: "Default Workspace" })).toBeInTheDocument();
    expect(screen.getByText("브라우저 검증 데이터")).toBeInTheDocument();
    expect(navigation.querySelectorAll("a")).toHaveLength(9);
    expect(
      screen.getByRole("link", { name: /Overview/ }),
    ).toHaveAttribute("aria-current", "page");
  });

  it("creates a workspace from the fleet dashboard and keeps it selected", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/workspaces";
    render(<App />);
    const selector = await screen.findByRole("combobox", { name: "Workspace" });

    await user.click(screen.getByRole("button", { name: "새 Workspace" }));
    await user.type(screen.getByRole("textbox", { name: "Workspace name" }), "Finance Ops");
    await user.click(screen.getByRole("button", { name: "Create" }));

    await waitFor(() => {
      expect(window.location.hash).toBe("#/overview");
      expect(selector).not.toHaveValue("00000000-0000-4000-8000-000000000001");
      expect((selector as HTMLSelectElement).selectedOptions[0]?.text).toBe("Finance Ops");
    });
    expect(window.localStorage.getItem("dbbackup.workspaceId")).toBe(
      (selector as HTMLSelectElement).value,
    );
  });
});

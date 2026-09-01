import { describe, expect, it, vi } from "vitest";
import { collectCursorPages } from "./pagination";

describe("collectCursorPages", () => {
  it("collects every cursor page before reporting a complete inventory", async () => {
    const fetchPage = vi.fn(async (cursor: string | null) => {
      if (cursor === null) return { items: [1], nextCursor: "page-2", totalCount: 3 };
      if (cursor === "page-2") return { items: [2], nextCursor: "page-3", totalCount: 3 };
      return { items: [3], nextCursor: null, totalCount: 3 };
    });

    await expect(collectCursorPages(fetchPage)).resolves.toEqual({
      items: [1, 2, 3],
      nextCursor: null,
      totalCount: 3,
      complete: true,
      pageCount: 3,
    });
  });

  it("marks the retained rows partial when the page cap is reached", async () => {
    const fetchPage = vi.fn(async (cursor: string | null) => ({
      items: [cursor ?? "page-1"],
      nextCursor: cursor === null ? "page-2" : "page-3",
      totalCount: 3,
    }));

    await expect(collectCursorPages(fetchPage, 2)).resolves.toEqual({
      items: ["page-1", "page-2"],
      nextCursor: "page-3",
      totalCount: 3,
      complete: false,
      pageCount: 2,
    });
  });

  it("fails closed on a repeated cursor or a mismatched total count", async () => {
    const repeated = await collectCursorPages(async (cursor) => ({
      items: [cursor ?? "first"],
      nextCursor: "stuck",
      totalCount: 3,
    }));
    const mismatched = await collectCursorPages(async () => ({
      items: [1],
      nextCursor: null,
      totalCount: 2,
    }));

    expect(repeated.complete).toBe(false);
    expect(repeated.pageCount).toBe(2);
    expect(mismatched.complete).toBe(false);
  });

  it("never reports complete when totalCount is missing or invalid at runtime", async () => {
    const missing = await collectCursorPages(async () => ({
      items: [1],
      nextCursor: null,
    } as never));
    const invalid = await collectCursorPages(async () => ({
      items: [1],
      nextCursor: null,
      totalCount: 1.5,
    }));

    expect(missing.complete).toBe(false);
    expect(missing.totalCount).toBeNull();
    expect(invalid.complete).toBe(false);
  });
});

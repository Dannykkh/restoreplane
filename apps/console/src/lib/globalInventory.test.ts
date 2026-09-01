import { describe, expect, it } from "vitest";
import { globalInventoryComplete } from "./globalInventory";

describe("bounded global read boundary", () => {
  it("accepts only inventories that explicitly prove completeness", () => {
    expect(globalInventoryComplete({ complete: true })).toBe(true);
    expect(globalInventoryComplete({ complete: false })).toBe(false);
    expect(globalInventoryComplete({})).toBe(false);
  });

  it("fails closed if any required global inventory is partial", () => {
    expect(
      globalInventoryComplete(
        { complete: true },
        { complete: false },
        { complete: true },
      ),
    ).toBe(false);
  });
});

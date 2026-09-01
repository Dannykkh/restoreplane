import { describe, expect, it } from "vitest";
import { cronPreview } from "./cron";

describe("cronPreview", () => {
  it("describes an hourly schedule with a fixed minute", () => {
    expect(cronPreview("15 * * * *")).toBe("매시간 15분");
  });
});

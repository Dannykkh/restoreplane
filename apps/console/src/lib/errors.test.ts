import { describe, expect, it } from "vitest";

import { ServiceError } from "./errors";

describe("plan publication readiness errors", () => {
  it.each([
    "PLAN_REVISION_TARGET_NOT_READY",
    "PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE",
    "PLAN_REVISION_DESTINATION_NOT_READY",
    "PLAN_REVISION_VERIFICATION_UNSUPPORTED",
  ])("keeps %s actionable instead of falling back to the generic service copy", (code) => {
    const error = new ServiceError(code);

    expect(error.message).not.toBe("서비스가 요청을 완료하지 못했습니다.");
    expect(error.action).not.toBe("Activity에서 요청 상태를 확인하고 다시 시도하세요.");
  });
});

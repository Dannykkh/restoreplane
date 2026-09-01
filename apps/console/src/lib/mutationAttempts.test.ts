import { describe, expect, it, vi } from "vitest";
import { ServiceError } from "./errors";
import { MutationAttemptRegistry } from "./mutationAttempts";

describe("MutationAttemptRegistry", () => {
  it("reuses one key after transport ambiguity, then clears it after success", async () => {
    const registry = new MutationAttemptRegistry();
    const invoke = vi.fn()
      .mockRejectedValueOnce(new ServiceError("IPC_PIPE_CONNECT_FAILED"))
      .mockResolvedValue("saved");
    const run = () => registry.run(
      "connection-save",
      "connection.save",
      { displayName: "primary" },
      invoke,
    );

    await expect(run()).rejects.toMatchObject({ code: "IPC_PIPE_CONNECT_FAILED" });
    await expect(run()).resolves.toBe("saved");
    await expect(run()).resolves.toBe("saved");

    expect(invoke.mock.calls[1]?.[0]).toBe(invoke.mock.calls[0]?.[0]);
    expect(invoke.mock.calls[2]?.[0]).not.toBe(invoke.mock.calls[1]?.[0]);
  });

  it("clears an authenticated ordinary failure and rotates on payload changes", async () => {
    const registry = new MutationAttemptRegistry();
    const invoke = vi.fn()
      .mockRejectedValueOnce(new ServiceError("APPLICATION_FORBIDDEN", {
        requestId: "request-authenticated",
      }))
      .mockResolvedValue("saved");

    await expect(registry.run(
      "destination-save",
      "destination.save",
      { displayName: "vault-a" },
      invoke,
    )).rejects.toMatchObject({ code: "APPLICATION_FORBIDDEN" });
    await expect(registry.run(
      "destination-save",
      "destination.save",
      { displayName: "vault-b" },
      invoke,
    )).resolves.toBe("saved");

    expect(invoke.mock.calls[1]?.[0]).not.toBe(invoke.mock.calls[0]?.[0]);
  });

  it("locks an outcome-unknown attempt until payload changes or the screen is re-entered", async () => {
    const registry = new MutationAttemptRegistry();
    const invoke = vi.fn()
      .mockRejectedValueOnce(new ServiceError("APPLICATION_OUTCOME_UNKNOWN", {
        requestId: "request-outcome-unknown",
      }))
      .mockResolvedValue("saved");

    const sameAttempt = () => registry.run(
      "plan-publish",
      "plan.publish",
      { planId: "plan-a", revisionId: "revision-1" },
      invoke,
    );
    await expect(sameAttempt()).rejects.toMatchObject({
      code: "APPLICATION_OUTCOME_UNKNOWN",
    });
    await expect(sameAttempt()).rejects.toMatchObject({
      code: "APPLICATION_OUTCOME_UNKNOWN",
    });
    expect(invoke).toHaveBeenCalledTimes(1);

    await expect(registry.run(
      "plan-publish",
      "plan.publish",
      { planId: "plan-a", revisionId: "revision-2" },
      invoke,
    )).resolves.toBe("saved");
    expect(invoke).toHaveBeenCalledTimes(2);
    expect(invoke.mock.calls[1]?.[0]).not.toBe(invoke.mock.calls[0]?.[0]);

    const reentered = new MutationAttemptRegistry();
    await expect(reentered.run(
      "plan-publish",
      "plan.publish",
      { planId: "plan-a", revisionId: "revision-1" },
      invoke,
    )).resolves.toBe("saved");
    expect(invoke).toHaveBeenCalledTimes(3);
  });

  it("also blocks incomplete server outcomes without retaining the raw secret payload", async () => {
    const registry = new MutationAttemptRegistry();
    const invoke = vi.fn().mockRejectedValue(new ServiceError("IPC_REQUEST_INCOMPLETE", {
      requestId: "request-running",
    }));
    const payload = { username: "operator", password: "secret-canary" };

    await expect(registry.run(
      "connection-save",
      "connection.save",
      payload,
      invoke,
    )).rejects.toMatchObject({ code: "IPC_REQUEST_INCOMPLETE" });
    await expect(registry.run(
      "connection-save",
      "connection.save",
      payload,
      invoke,
    )).rejects.toMatchObject({ code: "IPC_REQUEST_INCOMPLETE" });

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(JSON.stringify(registry)).not.toContain("secret-canary");
  });
});

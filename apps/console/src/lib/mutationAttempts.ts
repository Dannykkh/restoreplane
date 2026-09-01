import { useRef } from "react";
import { asServiceError, type ServiceError } from "./errors";

type AttemptEntry = {
  signatureDigest: string;
  idempotencyKey: string;
  blockedError: ServiceError | null;
};

const OUTCOME_UNCERTAIN_CODES = new Set([
  "APPLICATION_OUTCOME_UNKNOWN",
  "IPC_REQUEST_INCOMPLETE",
]);

export class MutationAttemptRegistry {
  readonly #attempts = new Map<string, AttemptEntry>();

  async run<T>(
    attemptId: string,
    command: string,
    payload: object,
    invoke: (idempotencyKey: string) => Promise<T>,
  ): Promise<T> {
    const signatureDigest = mutationSignatureDigest(command, payload);
    let attempt = this.#attempts.get(attemptId);
    if (!attempt || attempt.signatureDigest !== signatureDigest) {
      attempt = {
        signatureDigest,
        idempotencyKey: createIdempotencyKey(),
        blockedError: null,
      };
      this.#attempts.set(attemptId, attempt);
    }
    if (attempt.blockedError) throw attempt.blockedError;

    try {
      const result = await invoke(attempt.idempotencyKey);
      if (this.#attempts.get(attemptId) === attempt) this.#attempts.delete(attemptId);
      return result;
    } catch (failure) {
      const error = asServiceError(failure);
      if (isMutationOutcomeUncertain(error)) {
        attempt.blockedError = error;
      } else if (error.requestId !== null && this.#attempts.get(attemptId) === attempt) {
        this.#attempts.delete(attemptId);
      }
      throw error;
    }
  }
}

export function isMutationOutcomeUncertain(error: ServiceError): boolean {
  return OUTCOME_UNCERTAIN_CODES.has(error.code);
}

export function useMutationAttemptRegistry(): MutationAttemptRegistry {
  const registry = useRef<MutationAttemptRegistry | null>(null);
  if (!registry.current) registry.current = new MutationAttemptRegistry();
  return registry.current;
}

export function mutationSignatureDigest(command: string, payload: object): string {
  const bytes = new TextEncoder().encode(`${command}\n${canonicalJson(payload)}`);
  return `${fnv1a64(bytes, 0xcbf29ce484222325n)}${fnv1a64(bytes, 0x84222325cbf29ce4n)}`;
}

function canonicalJson(value: unknown): string {
  if (value === null) return "null";
  if (typeof value === "string" || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "number") return Number.isFinite(value) ? JSON.stringify(value) : "null";
  if (Array.isArray(value)) {
    return `[${value.map((item) => canonicalJson(item === undefined ? null : item)).join(",")}]`;
  }
  if (typeof value === "object") {
    const record = value as Record<string, unknown>;
    const fields = Object.keys(record)
      .sort()
      .filter((key) => record[key] !== undefined)
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(record[key])}`);
    return `{${fields.join(",")}}`;
  }
  return "null";
}

function fnv1a64(bytes: Uint8Array, seed: bigint): string {
  let hash = seed;
  for (const byte of bytes) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return hash.toString(16).padStart(16, "0");
}

function createIdempotencyKey(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `request-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

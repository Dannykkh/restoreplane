// 검증 레벨 / 불변성 / 서명 배지 — DESIGN.md: 상태를 배지로 즉시 읽히게.
import type { ImmutabilityKind, VerifyLevel } from "../types";
import { IconCheck, IconLock } from "./Icons";

export function VerifyBadge({ level, result }: { level: VerifyLevel | null; result: string | null }) {
  if (level === null) return <span className="badge badge-warning">검증 대기</span>;
  const pass = result === "pass";
  const cls = pass ? "badge-success" : "badge-danger";
  return (
    <span className={`badge ${cls}`}>
      {pass && <IconCheck />}L{level} {pass ? "통과" : "실패"}
    </span>
  );
}

const IMMUT_LABEL: Record<ImmutabilityKind, string> = {
  none: "불변 없음",
  server_append_only: "추가 전용",
  object_lock_governance: "Object Lock",
  object_lock_compliance: "Compliance Lock",
};

export function ImmutabilityBadge({ kind }: { kind: ImmutabilityKind }) {
  if (kind === "none") return <span className="badge badge-neutral">불변 없음</span>;
  return (
    <span className="badge badge-immutable">
      <IconLock />
      {IMMUT_LABEL[kind]}
    </span>
  );
}

export function SignedBadge({ signed }: { signed: boolean }) {
  return signed ? (
    <span className="badge badge-immutable">
      <IconLock />
      서명됨
    </span>
  ) : (
    <span className="badge badge-danger">미서명</span>
  );
}

export function StatusBadge({ status }: { status: string }) {
  if (status === "success") return <span className="badge badge-success">성공</span>;
  if (status === "running") return <span className="badge badge-neutral">실행 중</span>;
  return <span className="badge badge-danger">실패</span>;
}

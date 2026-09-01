import type { BackupScope, DatabaseTarget } from "./types";

export function enrichBackupScopes(
  scopes: readonly BackupScope[],
  _targets: readonly DatabaseTarget[],
): BackupScope[] {
  // Scope recovery and drift evidence is scope-specific. A database Target
  // projection is not a valid fallback for an explicit-table selector.
  return scopes.map((scope) => ({ ...scope }));
}

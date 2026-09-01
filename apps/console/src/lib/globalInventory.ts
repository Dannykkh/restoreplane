interface CompletenessEvidence {
  complete?: boolean;
}

export function globalInventoryComplete(
  ...inventories: readonly CompletenessEvidence[]
): boolean {
  return inventories.length > 0 && inventories.every((item) => item.complete === true);
}

export interface CursorSlice<T> {
  items: T[];
  nextCursor: string | null;
  totalCount: number;
}

export interface CollectedCursorPage<T> {
  items: T[];
  nextCursor: string | null;
  totalCount: number | null;
  complete: boolean;
  pageCount: number;
}

export const INVENTORY_PAGE_LIMIT = 100;
export const INVENTORY_MAX_PAGES = 100;

export async function collectCursorPages<T>(
  fetchPage: (cursor: string | null) => Promise<CursorSlice<T>>,
  maxPages = INVENTORY_MAX_PAGES,
): Promise<CollectedCursorPage<T>> {
  if (!Number.isSafeInteger(maxPages) || maxPages < 1) {
    throw new Error("PAGINATION_MAX_PAGES_INVALID");
  }

  const items: T[] = [];
  const seenCursors = new Set<string>();
  let cursor: string | null = null;
  let nextCursor: string | null = null;
  let totalCount: number | null = null;
  let countConsistent = true;

  for (let pageCount = 1; pageCount <= maxPages; pageCount += 1) {
    const page = await fetchPage(cursor);
    items.push(...page.items);
    nextCursor = page.nextCursor;

    if (!Number.isSafeInteger(page.totalCount) || page.totalCount < 0) {
      countConsistent = false;
    } else if (totalCount === null) {
      totalCount = page.totalCount;
    } else if (totalCount !== page.totalCount) {
      countConsistent = false;
    }

    if (nextCursor === null) {
      const countComplete = totalCount === null || totalCount === items.length;
      return {
        items,
        nextCursor,
        totalCount,
        complete: countConsistent && countComplete,
        pageCount,
      };
    }

    if (nextCursor === cursor || seenCursors.has(nextCursor)) {
      return {
        items,
        nextCursor,
        totalCount,
        complete: false,
        pageCount,
      };
    }
    seenCursors.add(nextCursor);
    cursor = nextCursor;
  }

  return {
    items,
    nextCursor,
    totalCount,
    complete: false,
    pageCount: maxPages,
  };
}

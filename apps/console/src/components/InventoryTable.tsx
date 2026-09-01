import { useRef, type KeyboardEvent, type ReactNode } from "react";

export interface InventoryColumn<T> {
  key: string;
  header: string;
  render: (row: T) => ReactNode;
  className?: string;
  sort?: "ascending" | "descending" | "none";
}

interface InventoryTableProps<T> {
  ariaLabel: string;
  columns: readonly InventoryColumn<T>[];
  rows: readonly T[];
  rowKey: (row: T) => string;
  selectedId: string | null;
  onSelect: (row: T) => void;
  emptyMessage: string;
  caption?: string;
}

export function InventoryTable<T>({
  ariaLabel,
  columns,
  rows,
  rowKey,
  selectedId,
  onSelect,
  emptyMessage,
  caption,
}: InventoryTableProps<T>) {
  const rowRefs = useRef<Array<HTMLTableRowElement | null>>([]);

  function moveSelection(index: number, targetIndex: number): void {
    const nextIndex = Math.max(0, Math.min(rows.length - 1, targetIndex));
    const next = rows[nextIndex];
    if (!next || nextIndex === index) return;
    onSelect(next);
    rowRefs.current[nextIndex]?.focus();
  }

  function onRowKeyDown(event: KeyboardEvent<HTMLTableRowElement>, row: T, index: number): void {
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        moveSelection(index, index + 1);
        break;
      case "ArrowUp":
        event.preventDefault();
        moveSelection(index, index - 1);
        break;
      case "Home":
        event.preventDefault();
        moveSelection(index, 0);
        break;
      case "End":
        event.preventDefault();
        moveSelection(index, rows.length - 1);
        break;
      case "Enter":
      case " ":
        event.preventDefault();
        onSelect(row);
        break;
    }
  }

  return (
    <div className="table-scroll" data-empty={rows.length === 0}>
      <table aria-label={ariaLabel} className="inventory-table">
        {caption && <caption className="sr-only">{caption}</caption>}
        <thead>
          <tr>
            {columns.map((column) => (
              <th
                key={column.key}
                scope="col"
                className={column.className}
                aria-sort={column.sort}
              >
                {column.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, index) => {
            const id = rowKey(row);
            const selected = id === selectedId;
            return (
              <tr
                key={id}
                ref={(element) => {
                  rowRefs.current[index] = element;
                }}
                tabIndex={selected || (!selectedId && index === 0) ? 0 : -1}
                aria-selected={selected}
                data-selected={selected}
                onClick={() => onSelect(row)}
                onKeyDown={(event) => onRowKeyDown(event, row, index)}
              >
                {columns.map((column) => (
                  <td key={column.key} className={column.className}>
                    {column.render(row)}
                  </td>
                ))}
              </tr>
            );
          })}
          {rows.length === 0 && (
            <tr>
              <td colSpan={columns.length} className="table-empty">
                {emptyMessage}
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}

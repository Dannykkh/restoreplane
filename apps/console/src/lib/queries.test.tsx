import { act, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useDebouncedValue } from "./queries";

function Probe({ value }: { value: string }) {
  const debounced = useDebouncedValue(value, 250);
  return <output>{debounced}</output>;
}

describe("debounced query input", () => {
  afterEach(() => vi.useRealTimers());

  it("keeps typing immediate while delaying the inventory query value", () => {
    vi.useFakeTimers();
    const view = render(<Probe value="o" />);
    view.rerender(<Probe value="orders" />);

    expect(screen.getByText("o")).toBeInTheDocument();
    act(() => vi.advanceTimersByTime(249));
    expect(screen.getByText("o")).toBeInTheDocument();
    act(() => vi.advanceTimersByTime(1));
    expect(screen.getByText("orders")).toBeInTheDocument();
  });
});

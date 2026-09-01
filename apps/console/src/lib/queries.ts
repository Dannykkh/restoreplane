import { useCallback, useEffect, useRef, useState } from "react";
import { asServiceError, type ServiceError } from "./errors";

export type QueryStatus = "loading" | "ready" | "error";

export interface QueryState<T> {
  status: QueryStatus;
  data: T | null;
  error: ServiceError | null;
  fetchedAt: number | null;
  refresh: () => void;
}

export function useDebouncedValue<T>(value: T, delayMs = 250): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = window.setTimeout(() => setDebounced(value), delayMs);
    return () => window.clearTimeout(timer);
  }, [delayMs, value]);
  return debounced;
}

export function useServiceQuery<T>(
  key: string,
  loader: () => Promise<T>,
): QueryState<T> {
  const loaderRef = useRef(loader);
  loaderRef.current = loader;
  const [revision, setRevision] = useState(0);
  const [state, setState] = useState<Omit<QueryState<T>, "refresh">>({
    status: "loading",
    data: null,
    error: null,
    fetchedAt: null,
  });

  useEffect(() => {
    let active = true;
    setState((previous) => ({
      status: "loading",
      data: previous.data,
      error: null,
      fetchedAt: previous.fetchedAt,
    }));
    void loaderRef
      .current()
      .then((data) => {
        if (!active) return;
        setState({ status: "ready", data, error: null, fetchedAt: Date.now() });
      })
      .catch((error: unknown) => {
        if (!active) return;
        setState((previous) => ({
          status: "error",
          data: previous.data,
          error: asServiceError(error),
          fetchedAt: previous.fetchedAt,
        }));
      });
    return () => {
      active = false;
    };
  }, [key, revision]);

  const refresh = useCallback(() => setRevision((value) => value + 1), []);
  return { ...state, refresh };
}

// Active-operation state, in one place.
//
// The rules this file encodes are the ones that make a transient operation
// pleasant to live with:
//
//   * the active identity is kept in `sessionStorage`, so a page reload can pick
//     the operation back up;
//   * after a reload the server replays everything the browser missed, and the
//     stream continues live from there;
//   * if the server no longer knows the identity — it restarted — that is *not*
//     a failure of the work. The identity is dropped and durable state is
//     re-read instead.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import { ApiError } from "../api/client";
import { fetchOperation } from "../api/operations";
import { watchOperation, type OperationWatch } from "../api/sse";
import { isTerminal, type AcceptedOperation, type OperationSnapshot } from "../api/types";

const STORAGE_KEY = "mineral.activeOperation";

/** The operation that currently holds the workspace, when one was refused. */
export interface Busy {
  operationId: string;
  kind?: string;
}

interface OperationsValue {
  /** The operation being watched, if any. */
  active: OperationSnapshot | null;
  /** Set when a mutating request was refused because the workspace is held. */
  busy: Busy | null;
  /** Bumped whenever durable state may have changed, so pages re-read it. */
  version: number;
  /** Starts an operation and watches it. */
  run: (starter: () => Promise<AcceptedOperation>) => Promise<void>;
  /** Stops tracking the active operation. Durable state is not affected. */
  clearActive: () => void;
  /** Silences a `workspace_busy` notice. */
  clearBusy: () => void;
}

const OperationsContext = createContext<OperationsValue | null>(null);

export function OperationsProvider({ children }: { children: ReactNode }) {
  const [active, setActive] = useState<OperationSnapshot | null>(null);
  const [busy, setBusy] = useState<Busy | null>(null);
  const [version, setVersion] = useState(0);

  // The watcher lives in a ref: it is not render state, and re-rendering must
  // not open a second stream.
  const watch = useRef<OperationWatch | null>(null);

  const stopWatching = useCallback(() => {
    watch.current?.close();
    watch.current = null;
  }, []);

  /** Appends one progress line to the active snapshot. */
  const appendProgress = useCallback((id: string, line: OperationSnapshot["progress"][number]) => {
    setActive((current) => {
      if (current === null || current.id !== id) {
        return current;
      }
      if (current.progress.some((existing) => existing.sequence === line.sequence)) {
        return current;
      }
      return {
        ...current,
        state: current.state === "queued" ? "running" : current.state,
        progress: [...current.progress, line],
      };
    });
  }, []);

  /**
   * Reads the authoritative snapshot and keeps watching if it is still running.
   *
   * The stream is a notification; this is the record. Every terminal transition
   * ends here, which is why a result is never inferred from the last progress
   * line.
   */
  const settle = useCallback(
    async (id: string) => {
      stopWatching();
      try {
        const snapshot = await fetchOperation(id);
        setActive(snapshot);
        if (isTerminal(snapshot.state)) {
          // Durable state may have changed: a decision was recorded, a
          // publication landed. Let the pages re-read it.
          setVersion((current) => current + 1);
        } else {
          watch.current = watchOperation(id, {
            onProgress: (line) => appendProgress(id, line),
            onTerminal: () => void settle(id),
            onInterrupted: () => void settle(id),
          });
        }
      } catch (error) {
        if (error instanceof ApiError && error.isUnknownOperation) {
          // The server restarted. This is not a failure of the work, and saying
          // "publish failed" would be a lie: the operation simply is not known
          // to this process any more.
          window.sessionStorage.removeItem(STORAGE_KEY);
          setActive(null);
          setVersion((current) => current + 1);
          return;
        }
        throw error;
      }
    },
    [appendProgress, stopWatching],
  );

  const run = useCallback(
    async (starter: () => Promise<AcceptedOperation>) => {
      setBusy(null);
      try {
        const accepted = await starter();
        window.sessionStorage.setItem(STORAGE_KEY, accepted.operation_id);
        await settle(accepted.operation_id);
      } catch (error) {
        if (error instanceof ApiError && error.isWorkspaceBusy && error.activeOperationId) {
          // Not an error banner: it is a pointer at the operation that holds the
          // workspace, which is more useful than "busy".
          setBusy({
            operationId: error.activeOperationId,
            ...(error.activeOperationKind ? { kind: error.activeOperationKind } : {}),
          });
          return;
        }
        throw error;
      }
    },
    [settle],
  );

  const clearActive = useCallback(() => {
    window.sessionStorage.removeItem(STORAGE_KEY);
    stopWatching();
    setActive(null);
  }, [stopWatching]);

  const clearBusy = useCallback(() => setBusy(null), []);

  // Pick up where this browser left off.
  useEffect(() => {
    const remembered = window.sessionStorage.getItem(STORAGE_KEY);
    if (remembered !== null) {
      void settle(remembered).catch(() => {
        // A snapshot we cannot read is not worth a crash on startup; the pages
        // show durable state regardless.
        window.sessionStorage.removeItem(STORAGE_KEY);
      });
    }
    return stopWatching;
  }, [settle, stopWatching]);

  const value = useMemo<OperationsValue>(
    () => ({ active, busy, version, run, clearActive, clearBusy }),
    [active, busy, version, run, clearActive, clearBusy],
  );

  return <OperationsContext.Provider value={value}>{children}</OperationsContext.Provider>;
}

/** The active operation, and the ability to start one. */
export function useOperations(): OperationsValue {
  const value = useContext(OperationsContext);
  if (value === null) {
    throw new Error("useOperations must be used inside an OperationsProvider");
  }
  return value;
}

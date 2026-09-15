// Watching an operation, wrapped once.
//
// No component touches `EventSource`. This is the only place that knows the
// event names, and the only place that has to get the lifecycle right:
//
//   * the server sends `id: <sequence>`, so a browser that reconnects
//     automatically sends `Last-Event-ID` and is replayed only what it missed;
//   * the server ends the stream after a terminal event, so the browser's own
//     reconnect would immediately open a stream that is already finished —
//     which is why a terminal event closes the source explicitly;
//   * a closed source is not a failure. The snapshot is authoritative, so the
//     caller reads it after the stream ends.

import { eventsUrl } from "./client";
import type { OperationState, ProgressEvent } from "./types";

/** What a watcher reports. */
export interface OperationWatchHandlers {
  /** One progress line, in order. */
  onProgress?: (event: ProgressEvent) => void;
  /**
   * The operation reached a terminal state.
   *
   * This is a *notification*: the caller must read
   * `GET /operations/:id` for the result or the failure.
   */
  onTerminal?: (state: OperationState) => void;
  /**
   * The stream ended without a terminal event — a dropped connection, or a
   * server that restarted. The caller should re-read the snapshot, which will
   * either still be there or answer `operation_not_found`.
   */
  onInterrupted?: () => void;
}

export interface OperationWatch {
  close(): void;
}

/** Watches one operation until it finishes or the caller stops watching. */
export function watchOperation(
  operationId: string,
  handlers: OperationWatchHandlers,
): OperationWatch {
  const source = new EventSource(eventsUrl(operationId));
  let settled = false;

  source.addEventListener("progress", (event) => {
    const data = (event as MessageEvent<string>).data;
    try {
      handlers.onProgress?.(JSON.parse(data) as ProgressEvent);
    } catch {
      // A line we cannot parse is not worth breaking the stream over; the
      // snapshot will carry the same information in a form we can read.
    }
  });

  const finish = (state: OperationState) => {
    settled = true;
    handlers.onTerminal?.(state);
    // The server has nothing more to send, and letting `EventSource` reconnect
    // would replay a finished operation forever.
    source.close();
  };

  source.addEventListener("completed", () => finish("succeeded"));
  source.addEventListener("failed", () => finish("failed"));

  source.addEventListener("error", () => {
    if (settled) {
      return;
    }
    // `EventSource` is already reconnecting unless it is closed. Report the
    // interruption so the caller can re-read the snapshot; do not treat it as a
    // failure of the work, which is a different thing entirely.
    if (source.readyState === EventSource.CLOSED) {
      handlers.onInterrupted?.();
    }
  });

  return {
    close: () => {
      settled = true;
      source.close();
    },
  };
}

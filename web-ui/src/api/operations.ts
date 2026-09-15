import { api } from "./client";
import type { AcceptedOperation, OperationSnapshot, OperationSummary } from "./types";

/** Everything this server still remembers, oldest first. */
export function fetchOperations(signal?: AbortSignal): Promise<OperationSummary[]> {
  return api.get<OperationSummary[]>("/operations", signal);
}

/** The authoritative state of one operation. */
export function fetchOperation(
  id: string,
  signal?: AbortSignal,
): Promise<OperationSnapshot> {
  return api.get<OperationSnapshot>(`/operations/${encodeURIComponent(id)}`, signal);
}

/**
 * Long-running work, started and left to run.
 *
 * Each returns an identity immediately; the work continues on the server.
 */
export const start = {
  publish: (): Promise<AcceptedOperation> => api.post<AcceptedOperation>("/operations/publish"),
  backup: (): Promise<AcceptedOperation> => api.post<AcceptedOperation>("/operations/backup"),
  backupInit: (): Promise<AcceptedOperation> =>
    api.post<AcceptedOperation>("/operations/backup/init"),
  backupVerify: (): Promise<AcceptedOperation> =>
    api.post<AcceptedOperation>("/operations/backup/verify"),
};

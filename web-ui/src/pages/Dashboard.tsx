// The dashboard: what this workspace is, and the two things you do to it.

import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";

import { fetchStatus } from "../api/status";
import { start } from "../api/operations";
import type { StatusResponse } from "../api/types";
import { BusyNotice } from "../components/BusyNotice";
import { Loading, Problem } from "../components/Async";
import { OperationCard, stateLabel } from "../components/OperationCard";
import { useOperations } from "../state/operations";

export function Dashboard() {
  const { active, version, run, clearActive } = useOperations();
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [starting, setStarting] = useState<string | null>(null);

  // `version` changes whenever an operation finishes, so this effect doubles as
  // "re-read durable state after a mutation".
  useEffect(() => {
    const controller = new AbortController();
    fetchStatus(controller.signal)
      .then((value) => {
        setStatus(value);
        setError(null);
      })
      .catch((cause: unknown) => {
        if (!controller.signal.aborted) {
          setError(cause);
        }
      });
    return () => controller.abort();
  }, [version]);

  const begin = useCallback(
    async (label: string, starter: () => ReturnType<typeof start.publish>) => {
      setStarting(label);
      try {
        await run(starter);
      } catch (cause) {
        setError(cause);
      } finally {
        setStarting(null);
      }
    },
    [run],
  );

  return (
    <div className="page">
      <h1>Mineral</h1>
      <BusyNotice />

      {error !== null && status === null ? (
        <Problem error={error} what="status" />
      ) : status === null ? (
        <Loading what="status" />
      ) : (
        <>
          <section className="card facts">
            <div className="fact">
              <span className="label">Source</span>
              <span>
                <span className="badge badge-ok">{status.source.kind}</span>{" "}
                <span className="mono small">{status.source.description}</span>
              </span>
            </div>
            <div className="fact">
              <span className="label">Publication</span>
              <span>
                <span className="mono">
                  {status.publication.remote} {status.publication.reference}
                </span>
                <span className="muted small">
                  {" "}
                  {status.publication.last_run
                    ? `last run ${status.publication.last_run}`
                    : "never published"}
                </span>
              </span>
            </div>
            <div className="fact">
              <span className="label">Backup</span>
              <span>
                {status.backup.configured ? (
                  <span className="badge badge-ok">configured</span>
                ) : (
                  <span className="badge">not configured</span>
                )}
              </span>
            </div>
            <div className="fact">
              <span className="label">Reviews</span>
              <span>
                <Link to="/reviews">
                  {status.reviews.pending_markdown} Markdown,{" "}
                  {status.reviews.pending_assets} asset
                  {status.reviews.pending_assets === 1 ? "" : "s"} pending
                </Link>
              </span>
            </div>
            <div className="fact">
              <span className="label">State</span>
              <span className="mono small">{status.state_path}</span>
            </div>
          </section>

          <section className="actions">
            <button
              type="button"
              className="button primary"
              disabled={starting !== null}
              onClick={() => void begin("publish", start.publish)}
            >
              {starting === "publish" ? "Starting…" : "Publish"}
            </button>
            <button
              type="button"
              className="button"
              disabled={starting !== null || !status.backup.configured}
              onClick={() => void begin("backup", start.backup)}
              title={status.backup.configured ? undefined : "This workspace has no backup target"}
            >
              {starting === "backup" ? "Starting…" : "Backup"}
            </button>
            <Link className="button" to="/doctor">
              Doctor
            </Link>
          </section>

          {active !== null ? (
            <>
              <h2>Current operation</h2>
              <OperationCard
                operation={active}
                onDismiss={active.state === "running" || active.state === "queued" ? undefined : clearActive}
              />
            </>
          ) : (
            <p className="muted">No operation is being watched.</p>
          )}

          {active !== null && !status.backup.configured && active.kind === "backup" ? (
            <p className="muted small">
              This workspace has no backup target; the operation reports that as a
              value rather than a failure.
            </p>
          ) : null}
          {active !== null ? (
            <p className="muted small">{stateLabel(active.state)}</p>
          ) : null}
        </>
      )}
    </div>
  );
}

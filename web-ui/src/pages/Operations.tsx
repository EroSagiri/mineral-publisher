// Operations: the generic view.
//
// Every kind of work appears here — publish, backup, backup bootstrap, backup
// verification and human decisions — because the server models them all the
// same way. There is no publish-specific progress component.

import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";

import { fetchOperation, fetchOperations } from "../api/operations";
import type { OperationSnapshot, OperationSummary } from "../api/types";
import { Loading, Problem } from "../components/Async";
import { OperationCard } from "../components/OperationCard";
import { useOperations } from "../state/operations";

function Detail({ id }: { id: string }) {
  const [snapshot, setSnapshot] = useState<OperationSnapshot | null>(null);
  const [error, setError] = useState<unknown>(null);

  useEffect(() => {
    const controller = new AbortController();
    setSnapshot(null);
    setError(null);
    fetchOperation(id, controller.signal)
      .then(setSnapshot)
      .catch((cause: unknown) => {
        if (!controller.signal.aborted) {
          setError(cause);
        }
      });
    return () => controller.abort();
  }, [id]);

  if (error !== null) {
    // A 404 here means this server does not know the identity: it was evicted,
    // or it belonged to a process that restarted. Not a failure of the work.
    return (
      <>
        <Problem error={error} what={`operation ${id}`} />
        <p className="muted small">
          An operation is not durable. If the server restarted, read{" "}
          <Link to="/">the current state</Link> instead — the publication and backup
          runs it produced are recorded separately.
        </p>
      </>
    );
  }
  if (snapshot === null) {
    return <Loading what={id} />;
  }
  return <OperationCard operation={snapshot} />;
}

export function Operations() {
  const { id } = useParams<{ id?: string }>();
  const { version } = useOperations();
  const [summaries, setSummaries] = useState<OperationSummary[] | null>(null);
  const [error, setError] = useState<unknown>(null);

  useEffect(() => {
    const controller = new AbortController();
    fetchOperations(controller.signal)
      .then(setSummaries)
      .catch((cause: unknown) => {
        if (!controller.signal.aborted) {
          setError(cause);
        }
      });
    return () => controller.abort();
  }, [version]);

  return (
    <div className="page">
      <h1>Operations</h1>
      {error !== null ? <Problem error={error} what="operations" /> : null}
      {summaries === null ? (
        <Loading what="operations" />
      ) : (
        <div className="split">
          <aside>
            {summaries.length === 0 ? (
              <p className="muted small">
                Nothing yet. Operations are not persisted: this list is what the
                running server remembers, and the newest finished ones are kept.
              </p>
            ) : (
              <ul className="queue">
                {summaries
                  .slice()
                  .reverse()
                  .map((summary) => (
                    <li key={summary.id}>
                      <Link
                        to={`/operations/${summary.id}`}
                        className={
                          summary.id === id ? "queue-item selected" : "queue-item"
                        }
                      >
                        <span className="mono small">{summary.id}</span>
                        <span className="kind small">{summary.kind}</span>
                        <span className={`badge badge-${summary.state} small`}>
                          {summary.state}
                        </span>
                      </Link>
                    </li>
                  ))}
              </ul>
            )}
          </aside>
          <main>
            {id === undefined ? (
              <p className="muted">Choose an operation.</p>
            ) : (
              <Detail id={id} />
            )}
          </main>
        </div>
      )}
    </div>
  );
}

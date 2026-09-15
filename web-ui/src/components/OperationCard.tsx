// One component for every operation.
//
// Publish, backup, a backup bootstrap and a human decision all render here.
// That is the payoff of the operation API: there is no publish-specific
// progress bar and no review-specific one.

import { Link } from "react-router-dom";

import type { OperationSnapshot } from "../api/types";
import { isTerminal } from "../api/types";

/** A stable, human word for a state. */
export function stateLabel(state: OperationSnapshot["state"]): string {
  switch (state) {
    case "queued":
      return "Queued";
    case "running":
      return "Running";
    case "succeeded":
      return "Succeeded";
    case "failed":
      return "Failed";
  }
}

function time(ms: number | undefined): string | null {
  if (ms === undefined) {
    return null;
  }
  return new Date(ms).toLocaleTimeString();
}

/** The line that says what an operation produced. */
function resultLine(operation: OperationSnapshot): string | null {
  const result = operation.result;
  if (result === undefined) {
    return null;
  }
  if (result.publish) {
    return `publication: ${result.publish.status}`;
  }
  if (result.backup) {
    return `backup: ${result.backup.status}`;
  }
  if (result.verify) {
    return `verification: ${result.verify.status}`;
  }
  if (result.backup_init) {
    return `backup ref: ${result.backup_init.status}`;
  }
  if (result.doctor) {
    return `health: ${result.doctor.ok ? "ok" : "failed"}`;
  }
  if (result.review) {
    return `decision: ${result.review.decision}${result.review.already ? " (already recorded)" : ""}`;
  }
  return null;
}

export function OperationCard({
  operation,
  onDismiss,
}: {
  operation: OperationSnapshot;
  onDismiss?: () => void;
}) {
  const finished = time(operation.finished_at_ms);
  const result = resultLine(operation);

  return (
    <section className={`card operation operation-${operation.state}`}>
      <header className="operation-header">
        <div>
          <Link to={`/operations/${operation.id}`} className="mono">
            {operation.id}
          </Link>
          <span className="kind">{operation.kind}</span>
        </div>
        <div className="operation-state">
          <span className={`badge badge-${operation.state}`}>{stateLabel(operation.state)}</span>
          {onDismiss && isTerminal(operation.state) ? (
            <button type="button" className="link" onClick={onDismiss}>
              dismiss
            </button>
          ) : null}
        </div>
      </header>

      {operation.progress.length > 0 ? (
        <ol className="progress">
          {operation.progress.map((line) => (
            <li key={line.sequence} className={line.kind === "stage" ? "stage" : "detail"}>
              {line.message}
            </li>
          ))}
        </ol>
      ) : (
        <p className="muted">
          {isTerminal(operation.state) ? "No progress was reported." : "Starting…"}
        </p>
      )}

      {result !== null ? <p className="result">{result}</p> : null}

      {operation.failure ? (
        <div className="failure">
          <p>
            <span className="mono code">{operation.failure.code}</span>{" "}
            {operation.failure.message}
          </p>
          {operation.failure.no_base_commit ? (
            <p className="muted">
              The backup ref{" "}
              <span className="mono">
                {operation.failure.no_base_commit.remote}{" "}
                {operation.failure.no_base_commit.reference}
              </span>{" "}
              does not exist yet. Run “Initialize backup ref” once.
            </p>
          ) : null}
          {operation.failure.causes.length > 0 ? (
            <ul className="causes">
              {operation.failure.causes.map((cause, index) => (
                <li key={index}>{cause}</li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}

      <footer className="muted small">
        started {time(operation.started_at_ms) ?? "—"}
        {finished !== null ? ` · finished ${finished}` : ""}
      </footer>
    </section>
  );
}

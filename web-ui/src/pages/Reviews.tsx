// The review queue — the page that most benefits from being a GUI.
//
// A decision is *not* reported as done when the request returns. The request
// returns an operation; the queue is refreshed when that operation finishes.

import { useCallback, useEffect, useState } from "react";
import { NavLink, useParams } from "react-router-dom";

import { decide, fetchReview, fetchReviews } from "../api/reviews";
import type { ReviewDetail, ReviewSummary } from "../api/types";
import { BusyNotice } from "../components/BusyNotice";
import { Loading, Problem } from "../components/Async";
import { useOperations } from "../state/operations";

/** One waiting attempt, as a link into the detail view. */
function QueueGroup({
  title,
  attempts,
}: {
  title: string;
  attempts: ReviewSummary[];
}) {
  return (
    <div className="queue-group">
      <h3>
        {title} <span className="muted small">{attempts.length}</span>
      </h3>
      {attempts.length === 0 ? (
        <p className="muted small">Nothing waiting.</p>
      ) : (
        <ul className="queue">
          {attempts.map((attempt) => (
            <li key={attempt.attempt}>
              <NavLink
                to={`/reviews/${encodeURIComponent(attempt.attempt)}`}
                className={({ isActive }) => (isActive ? "queue-item selected" : "queue-item")}
              >
                <span className="mono small">{attempt.attempt}</span>
                <span className="path">{attempt.path}</span>
              </NavLink>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Detail({ attempt }: { attempt: string }) {
  const { run } = useOperations();
  const [detail, setDetail] = useState<ReviewDetail | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [deciding, setDeciding] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setDetail(null);
    setError(null);
    fetchReview(attempt, controller.signal)
      .then(setDetail)
      .catch((cause: unknown) => {
        if (!controller.signal.aborted) {
          setError(cause);
        }
      });
    return () => controller.abort();
  }, [attempt]);

  const submit = useCallback(
    async (decision: "approve" | "reject") => {
      setDeciding(decision);
      setError(null);
      try {
        // This resolves as soon as the server accepts the *operation*. The page
        // does not claim the decision is recorded yet — the operation card on
        // the dashboard reports what actually happened.
        await run(() => decide(attempt, decision));
      } catch (cause) {
        setError(cause);
      } finally {
        setDeciding(null);
      }
    },
    [attempt, run],
  );

  if (error !== null) {
    return <Problem error={error} what={attempt} />;
  }
  if (detail === null) {
    return <Loading what={attempt} />;
  }

  return (
    <section className="card detail">
      <header>
        <span className="mono">{detail.attempt}</span>
        <span className="badge">{detail.subject}</span>
      </header>

      <dl className="fields">
        <dt>Path</dt>
        <dd className="mono">{detail.path}</dd>
        <dt>Content</dt>
        <dd className="mono small">{detail.sha256}</dd>
        <dt>Decision</dt>
        <dd>
          <span className="mono code">{detail.decision.code}</span>
          <div className="muted small">{detail.decision.detail}</div>
        </dd>
        <dt>Policy</dt>
        <dd className="mono small">
          {detail.policy.name} {detail.policy.version}
        </dd>
        <dt>Human</dt>
        <dd>
          {detail.human_resolution === "resolved" ? (
            <span className="badge badge-ok">resolved</span>
          ) : (
            <span className="badge badge-queued">pending</span>
          )}
        </dd>
      </dl>

      {detail.reason_codes && detail.reason_codes.length > 0 ? (
        <div className="reasons">
          <h4>Reviewer reasons</h4>
          <ul>
            {detail.reason_codes.map((code) => (
              <li key={code} className="mono small">
                {code}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {detail.summary ? <p className="summary">{detail.summary}</p> : null}

      <div className="actions">
        <button
          type="button"
          className="button"
          disabled={deciding !== null}
          onClick={() => void submit("reject")}
        >
          {deciding === "reject" ? "Starting…" : "Reject"}
        </button>
        <button
          type="button"
          className="button primary"
          disabled={deciding !== null}
          onClick={() => void submit("approve")}
        >
          {deciding === "approve" ? "Starting…" : "Approve"}
        </button>
      </div>
    </section>
  );
}

export function Reviews() {
  const { attempt } = useParams<{ attempt?: string }>();
  const { version } = useOperations();
  const [queues, setQueues] = useState<{
    markdown: ReviewSummary[];
    assets: ReviewSummary[];
  } | null>(null);
  const [error, setError] = useState<unknown>(null);

  useEffect(() => {
    const controller = new AbortController();
    fetchReviews(controller.signal)
      .then(setQueues)
      .catch((cause: unknown) => {
        if (!controller.signal.aborted) {
          setError(cause);
        }
      });
    return () => controller.abort();
  }, [version]);

  return (
    <div className="page">
      <h1>Reviews</h1>
      <BusyNotice />
      {error !== null ? <Problem error={error} what="the review queue" /> : null}
      {queues === null ? (
        <Loading what="the review queue" />
      ) : (
        <div className="split">
          <aside>
            <QueueGroup title="Markdown" attempts={queues.markdown} />
            <QueueGroup title="Assets" attempts={queues.assets} />
          </aside>
          <main>
            {attempt === undefined ? (
              <p className="muted">Choose an attempt from the queue.</p>
            ) : (
              <Detail attempt={attempt} />
            )}
          </main>
        </div>
      )}
    </div>
  );
}

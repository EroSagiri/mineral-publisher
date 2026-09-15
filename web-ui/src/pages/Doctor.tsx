// The health scan, as a checklist.
//
// `/doctor` already returns structured checks, so this renders them. Nothing
// here parses CLI text.

import { useEffect, useState } from "react";

import { fetchDoctor } from "../api/status";
import type { DoctorResponse } from "../api/types";
import { Loading, Problem } from "../components/Async";

export function Doctor() {
  const [report, setReport] = useState<DoctorResponse | null>(null);
  const [error, setError] = useState<unknown>(null);

  useEffect(() => {
    const controller = new AbortController();
    fetchDoctor(controller.signal)
      .then(setReport)
      .catch((cause: unknown) => {
        if (!controller.signal.aborted) {
          setError(cause);
        }
      });
    return () => controller.abort();
  }, []);

  if (error !== null) {
    return (
      <div className="page">
        <h1>Doctor</h1>
        <Problem error={error} what="the health scan" />
      </div>
    );
  }
  if (report === null) {
    return (
      <div className="page">
        <h1>Doctor</h1>
        <Loading what="the health scan" />
      </div>
    );
  }

  return (
    <div className="page">
      <h1>Doctor</h1>
      <p>
        {report.ok ? (
          <span className="badge badge-ok">all checks passed</span>
        ) : (
          <span className="badge badge-failed">{report.failed} failed</span>
        )}{" "}
        <span className="muted small">{report.passed} passed</span>
      </p>
      <table className="checks">
        <tbody>
          {report.checks.map((check) => (
            <tr key={check.name} className={check.ok ? "" : "failed"}>
              <td className="check-mark">{check.ok ? "✓" : "✗"}</td>
              <td>{check.name}</td>
              <td className="muted small mono">{check.detail}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

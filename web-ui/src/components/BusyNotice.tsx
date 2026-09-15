// `workspace_busy` is a constraint, not an error.
//
// One workspace runs one mutating operation at a time. Telling the operator
// "another operation is running" and offering to show it turns a backend rule
// into something that makes sense.

import { Link } from "react-router-dom";

import { useOperations } from "../state/operations";

export function BusyNotice() {
  const { busy, clearBusy } = useOperations();
  if (busy === null) {
    return null;
  }
  return (
    <div className="notice">
      <div>
        <strong>Another operation is running.</strong>{" "}
        <span className="muted">
          Only one change can be made at a time{busy.kind ? ` (${busy.kind})` : ""}.
        </span>
      </div>
      <div className="notice-actions">
        <Link to={`/operations/${busy.operationId}`} className="button">
          View current operation
        </Link>
        <button type="button" className="link" onClick={clearBusy}>
          dismiss
        </button>
      </div>
    </div>
  );
}

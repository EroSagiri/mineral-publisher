// The three states every read has, in one place.

import type { ReactNode } from "react";

export function Loading({ what }: { what: string }) {
  return <p className="muted">Loading {what}…</p>;
}

export function Problem({ error, what }: { error: unknown; what: string }) {
  const message = error instanceof Error ? error.message : String(error);
  return (
    <div className="failure">
      <p>Could not load {what}.</p>
      <p className="muted">{message}</p>
    </div>
  );
}

export function Ready({ children }: { children: ReactNode }) {
  return <>{children}</>;
}

import { api } from "./client";
import type { AcceptedOperation, ReviewDetail, ReviewListResponse } from "./types";

/** Every attempt waiting for a human decision. */
export function fetchReviews(signal?: AbortSignal): Promise<ReviewListResponse> {
  return api.get<ReviewListResponse>("/reviews", signal);
}

/** One attempt, with the facts a decision needs. */
export function fetchReview(attempt: string, signal?: AbortSignal): Promise<ReviewDetail> {
  return api.get<ReviewDetail>(`/reviews/${encodeURIComponent(attempt)}`, signal);
}

/**
 * Record a decision.
 *
 * This returns as soon as the server has accepted the *operation* — it does not
 * mean the decision is durable. The caller watches the operation and refreshes
 * the queue when it finishes.
 */
export function decide(
  attempt: string,
  decision: "approve" | "reject",
): Promise<AcceptedOperation> {
  return api.post<AcceptedOperation>(
    `/reviews/${encodeURIComponent(attempt)}/${decision}`,
  );
}

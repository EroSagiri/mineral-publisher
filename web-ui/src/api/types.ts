// The wire types, mirroring the server's `Web*` DTOs.
//
// Two properties are deliberate:
//
// 1. **No secret-shaped field exists on any type here.** Not "we do not render
//    it" — there is no field to render. The server guarantees the same thing
//    structurally, and this file is the other half of that guarantee: a
//    credential cannot reach the browser because neither end has a place to put
//    one.
// 2. These are hand-written, not generated from Rust types. The HTTP API is a
//    compatibility contract, so it changes when this file changes.

/** Where an operation is in its life. */
export type OperationState = "queued" | "running" | "succeeded" | "failed";

/** What an operation does. */
export type OperationKind =
  | "publish"
  | "backup"
  | "verify_backup"
  | "backup_init"
  | "doctor"
  | "review_decision";

export interface SourceSummary {
  id: string;
  kind: "local" | "r2";
  description: string;
}

export interface PublicationTarget {
  remote: string;
  reference: string;
  last_run?: string;
}

export interface ReviewCounts {
  pending_markdown: number;
  pending_assets: number;
}

/** Presence only. There is no field for an endpoint or a credential. */
export interface BackupStatus {
  configured: boolean;
}

export interface StatusResponse {
  configured: boolean;
  source: SourceSummary;
  state_path: string;
  publication: PublicationTarget;
  reviews: ReviewCounts;
  backup: BackupStatus;
}

export interface DoctorCheck {
  name: string;
  ok: boolean;
  detail: string;
}

export interface DoctorResponse {
  ok: boolean;
  passed: number;
  failed: number;
  checks: DoctorCheck[];
}

export interface ReviewSummary {
  attempt: string;
  path: string;
}

export interface ReviewListResponse {
  markdown: ReviewSummary[];
  assets: ReviewSummary[];
}

export interface Decision {
  code: string;
  detail: string;
}

export interface Policy {
  name: string;
  version: string;
  hash: string;
}

export interface ReviewDetail {
  attempt: string;
  subject: string;
  path: string;
  sha256: string;
  decision: Decision;
  policy: Policy;
  reason_codes?: string[];
  summary?: string;
  human_resolution: "resolved" | "pending";
}

export interface ProgressEvent {
  sequence: number;
  kind: "stage" | "detail";
  message: string;
}

export interface NoBaseCommit {
  remote: string;
  reference: string;
  snapshot_id: string;
  files: number;
}

/** A failure, with a stable code to switch on and a message for a human. */
export interface OperationFailure {
  code: string;
  message: string;
  causes: string[];
  no_base_commit?: NoBaseCommit;
}

export interface PublishResult {
  status: string;
  git?: string;
  snapshot_id: string;
  files: number;
  run_id?: string;
  markdown: number;
  assets: number;
  public_scope: string;
}

export interface BackupResult {
  status: string;
  snapshot_id?: string;
  files?: number;
  lfs_objects?: number;
  run_id?: string;
  remote?: string;
  reference?: string;
}

export interface VerifyResult {
  status: string;
  commit?: string;
  files?: number;
  lfs_objects?: number;
}

export interface BackupInitResult {
  status: string;
  commit?: string;
  remote?: string;
  reference?: string;
}

export interface ReviewResolution {
  attempt: string;
  decision: string;
  already: boolean;
}

export interface OperationResult {
  kind: OperationKind;
  publish?: PublishResult;
  backup?: BackupResult;
  verify?: VerifyResult;
  backup_init?: BackupInitResult;
  doctor?: DoctorResponse;
  review?: ReviewResolution;
}

/** The authoritative state of one operation. */
export interface OperationSnapshot {
  id: string;
  kind: OperationKind;
  state: OperationState;
  queued_at_ms: number;
  started_at_ms?: number;
  finished_at_ms?: number;
  progress: ProgressEvent[];
  result?: OperationResult;
  failure?: OperationFailure;
}

export interface OperationSummary {
  id: string;
  kind: OperationKind;
  state: OperationState;
  started_at_ms?: number;
  finished_at_ms?: number;
  failure_code?: string;
}

export interface AcceptedOperation {
  operation_id: string;
  kind: OperationKind;
  state: OperationState;
}

/** The body of every error response. */
export interface ApiErrorBody {
  code: string;
  message: string;
  causes?: string[];
  active_operation_id?: string;
  active_operation_kind?: string;
}

export function isTerminal(state: OperationState): boolean {
  return state === "succeeded" || state === "failed";
}

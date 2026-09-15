import { api } from "./client";
import type { DoctorResponse, StatusResponse } from "./types";

/** What this workspace holds. */
export function fetchStatus(signal?: AbortSignal): Promise<StatusResponse> {
  return api.get<StatusResponse>("/status", signal);
}

/** The health scan, as a checklist. */
export function fetchDoctor(signal?: AbortSignal): Promise<DoctorResponse> {
  return api.get<DoctorResponse>("/doctor", signal);
}

export type AdPlatform = "meta" | "google" | "tiktok";

export type AdConnection = {
  platform: AdPlatform;
  label: string;
  /** The server has this platform's app credentials. */
  available: boolean;
  status: "not_connected" | "connected" | "needs_reconnect";
  connected_as: string | null;
  connected_at: string | null;
  token_expires_at: string | null;
  /** The sign-in runs out within a week: reconnect before then. */
  expires_soon: boolean;
  error: string | null;
  /** After a disconnect: whether the platform confirmed it revoked DumbMail's access. */
  last_revoke: "confirmed" | "not_confirmed" | null;
  /** Google: people type their customer ID (and manager ID). */
  manual_account_entry: boolean;
  /** False until someone confirms real matching and removal on a live account. */
  verified_live: boolean;
  syncs: number;
};

export type AdSettings = {
  google_consent: boolean;
  google_consent_at: string | null;
  google_consent_by: string | null;
};

export type AdAccount = { id: string; name: string | null };

export type MatchFields = "contact" | "contact_and_location";

export type PauseReason = "user" | "google_consent" | "tiktok_minimum" | "audience_missing" | "create_unclear";

export type AudienceSync = {
  id: string;
  platform: AdPlatform;
  platform_label: string;
  ad_account_id: string;
  ad_account_name: string | null;
  login_account_id: string | null;
  source: { type: "list" | "segment"; id: string; name: string | null; exists: boolean };
  name: string;
  audience_id: string | null;
  status: "active" | "paused";
  pause_reason: PauseReason | null;
  pause_message: string | null;
  running: boolean;
  connection_status: "connected" | "needs_reconnect" | "disconnected";
  match_fields: MatchFields;
  interval_hours: number;
  next_run_at: string | null;
  last_run_at: string | null;
  last_success_at: string | null;
  error_code: string | null;
  error: string | null;
  /** Hashed identifiers sent and possibly in the audience. */
  sent: number;
  /** Of those, the ones the platform confirmed. */
  confirmed: number;
  /** Removals the platform hasn't confirmed yet. */
  pending_removal: number;
  last_added: number;
  last_removed: number;
  platform_size: number | null;
  platform_size_upper: number | null;
  match_rate: number | null;
  platform_status: string | null;
  verified_live: boolean;
  created_at: string;
  updated_at: string;
};

export type AudienceRun = {
  id: string;
  trigger: "created" | "scheduled" | "manual";
  mode: "full" | "remove_only" | "veto_only";
  status: "running" | "done" | "failed" | "stopped";
  phase: string;
  added: number;
  removed: number;
  rejected: number;
  skipped: number;
  error_code: string | null;
  error: string | null;
  started_at: string;
  finished_at: string | null;
};

export type WaitingBatch = { op: "add" | "remove"; status: "pending" | "accepted"; count: number };

export type AudienceSyncDetail = AudienceSync & { runs: AudienceRun[]; waiting: WaitingBatch[] };

export type CreateSyncInput = {
  platform: AdPlatform;
  ad_account_id: string;
  login_account_id?: string;
  source_type: "list" | "segment";
  source_id: string;
  name?: string;
  match_fields: MatchFields;
  interval_hours: number;
  accept_terms: boolean;
};

export type UpdateSyncInput = Partial<Pick<AudienceSync, "status" | "interval_hours" | "match_fields">> & {
  recreate_audience?: boolean;
  confirm?: boolean;
};

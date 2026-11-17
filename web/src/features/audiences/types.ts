export type AdPlatform = "meta" | "google" | "tiktok";

export type AdConnection = {
  platform: AdPlatform;
  label: string;
  /** The server has this platform's app credentials. */
  available: boolean;
  status: "not_connected" | "connected" | "needs_reconnect";
  connected_as: string | null;
  connected_at: string | null;
  token_hint: string | null;
  token_expires_at: string | null;
  error: string | null;
  /** Google without a developer token: people type their customer ID. */
  manual_account_entry: boolean;
  syncs: number;
};

export type AdAccount = { id: string; name: string | null; manager: boolean };

export type MatchFields = "contact" | "contact_and_location";

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
  running: boolean;
  connection_status: "connected" | "needs_reconnect" | "disconnected";
  include_unsubscribed: boolean;
  match_fields: MatchFields;
  interval_hours: number;
  next_run_at: string | null;
  last_run_at: string | null;
  last_success_at: string | null;
  error: string | null;
  members_sent: number;
  last_added: number;
  last_removed: number;
  last_invalid: number;
  platform_size: number | null;
  platform_size_upper: number | null;
  match_rate: number | null;
  platform_status: string | null;
  created_at: string;
  updated_at: string;
};

export type AudienceRun = {
  id: string;
  trigger: "created" | "scheduled" | "manual";
  status: "running" | "done" | "failed" | "stopped";
  phase: string;
  added: number;
  removed: number;
  invalid: number;
  skipped: number;
  error: string | null;
  started_at: string;
  finished_at: string | null;
};

export type AudienceSyncDetail = AudienceSync & { runs: AudienceRun[] };

export type CreateSyncInput = {
  platform: AdPlatform;
  ad_account_id: string;
  login_account_id?: string;
  source_type: "list" | "segment";
  source_id: string;
  name?: string;
  include_unsubscribed: boolean;
  match_fields: MatchFields;
  interval_hours: number;
  accept_terms: boolean;
};

export type UpdateSyncInput = Partial<Pick<AudienceSync, "status" | "interval_hours" | "include_unsubscribed" | "match_fields">>;

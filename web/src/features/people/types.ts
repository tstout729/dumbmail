import type { Consent } from "../../api";

export type Channel = "email" | "sms";
/** A channel the suppression list shows (WhatsApp opt-outs are kept per number, read-only here). */
export type ListedChannel = Channel | "whatsapp";

export type ConsentEvent = {
  id: string;
  channel: ListedChannel | "ads" | "marketing";
  field: "consent" | "suppression" | "opt_out" | "hold" | "address";
  old_value: string | null;
  new_value: string | null;
  method: string;
  via: string | null;
  address: string | null;
  actor_email: string | null;
  form_name: string | null;
  message_subject: string | null;
  ip: string | null;
  user_agent: string | null;
  occurred_at: string;
  /** What changed, in words. */
  summary: string;
  /** How it happened, in words. */
  how: string;
};

export type Reason = { code: string; text: string; since?: string; how?: string };
export type ChannelReach = { can_send: boolean; reasons: Reason[] };
/** `whatsapp` is null when the account has no WhatsApp sender. */
export type Reachability = { email: ChannelReach; sms: ChannelReach; whatsapp: ChannelReach | null };

export type ProfileNote = { id: string; author_user_id: string | null; author: string; body: string; created_at: string };

export type PeopleSummary = {
  reachability: Reachability;
  ads_opt_out: boolean;
  ads_opt_out_at: string | null;
  ads_opt_out_source: string | null;
  consent_history: ConsentEvent[];
  consent_history_total: number;
  history_started_at: string;
  notes: ProfileNote[];
};

export type SuppressionReason = "unsubscribed" | "hard_bounce" | "complaint" | "manual" | "pending" | "not_subscribed" | "marketing_hold";

export type Suppression = {
  profile_id: string;
  channel: ListedChannel;
  address: string;
  first_name: string | null;
  last_name: string | null;
  reason: SuppressionReason;
  reason_text: string;
  /** When the recorded change happened; null for a block from before consent history started. */
  since: string | null;
  how: string | null;
  can_unsuppress: boolean;
  why_not: string | null;
};

export type SuppressionList = {
  data: Suppression[];
  total: number;
  counts: { channel: ListedChannel; reason: SuppressionReason; count: number }[];
  deleted_profiles_suppressed: number;
};

export type SuppressionQuery = { q?: string; channel?: ListedChannel; reason?: SuppressionReason; from?: string; to?: string; limit?: number; offset?: number };

export type BulkAction = {
  id: string;
  action: string;
  status: "queued" | "running" | "done" | "failed";
  total: number;
  processed: number;
  succeeded: number;
  skipped: number;
  error: string | null;
  file_url: string | null;
  created_by: string | null;
  via: string;
  created_at: string;
  finished_at: string | null;
};

export type SuppressResult = {
  dry_run?: boolean;
  count?: number;
  suppressed?: number;
  already_suppressed?: number;
  invalid: string[];
  invalid_count: number;
  bulk_action?: BulkAction;
};

export type DuplicateProfile = {
  id: string;
  email: string | null;
  phone_number: string | null;
  external_id: string | null;
  first_name: string | null;
  last_name: string | null;
  email_consent: Consent;
  sms_consent: Consent;
  created_at: string;
};

export type DuplicateGroup = {
  reason: "email" | "phone_number" | "external_id" | "linked_identifiers";
  why: string;
  key: unknown;
  times_seen?: number;
  profiles: DuplicateProfile[];
};

export type MergeField = { field: string; keep: unknown; merge: unknown; result: unknown; conflict: boolean };

export type MergeRow = {
  id: string;
  email: string | null;
  phone_number: string | null;
  external_id: string | null;
  first_name: string | null;
  last_name: string | null;
  properties: Record<string, unknown>;
  email_consent: Consent;
  sms_consent: Consent;
  email_suppressed_reason: string | null;
  sms_suppressed_reason: string | null;
  ads_opt_out: boolean;
};

export type MergePreview = {
  dry_run?: boolean;
  merged?: boolean;
  result: MergeRow;
  keep: MergeRow;
  merge: MergeRow;
  fields: MergeField[];
  rows_moving: Record<string, number>;
};

export type MergeInput = { keep_id: string; merge_id: string; choices: Record<string, "keep" | "merge">; confirm: boolean };

export type BulkSelection = { profile_ids?: string[]; q?: string; list_id?: string; segment_id?: string };

export type BulkActionName = "add_to_list" | "remove_from_list" | "set_property" | "clear_property" | "suppress" | "delete" | "export";

export type BulkInput = {
  action: BulkActionName;
  selection: BulkSelection;
  list_id?: string;
  key?: string;
  value?: unknown;
  channels?: Channel[];
  confirm?: boolean;
  /** The saved set from the preview; required to confirm. */
  set_id?: string;
};

export type BulkDryRun = { dry_run: true; count: number; set_id: string | null; expires_at?: string; destructive: boolean; note: string };

export type PrivacyFile = { id: string; name: string; url: string; expires_at?: string };

export type PrivacyRequest = {
  id: string;
  kind: "export" | "delete";
  status: "building" | "ready" | "failed" | "done";
  identified_by: "email" | "phone_number";
  requested_by: string | null;
  via: string;
  profiles_found: number;
  result: Record<string, unknown>;
  created_at: string;
  files: PrivacyFile[];
};

export type PersonInput = { email?: string; phone_number?: string; confirm?: boolean };

export type ExportResult = { request_id: string; status: "building" | "done"; found: number; note?: string };

export type DeleteResult = {
  dry_run?: boolean;
  found?: number;
  profiles?: { profile_id: string; events: number; messages: number; lists: number }[];
  note?: string;
  deleted?: number;
};

import type { Audience, Channel } from "../../api";

export type Timing = "all_at_once" | "local_time" | "best_time" | "gradual";
export type Late = "next_day" | "immediately";

/** A campaign's `send_options` (null: everyone at once, no smart sending). */
export type SendOptions = {
  timing: Timing;
  local_time?: string;
  late?: Late;
  window_start?: string;
  window_end?: string;
  within_hours?: number;
  spread_hours?: number;
  smart_sending: boolean;
  smart_sending_hours?: number;
  /** Set by the server when a local-time campaign is scheduled or sent. */
  local_date?: string;
};

/** The campaign fields this feature reads (the core Campaign type doesn't carry them). */
export type WithSendOptions = { send_options?: SendOptions | null };

export type SendPlanInput = {
  campaign_id?: string;
  channel: Channel;
  audience: Audience;
  send_options: SendOptions | null;
  send_at?: string;
};

export type SendPlanBucket = { at: string; count: number; zones: string[] };

export type SendPlanPreview = {
  total: number;
  starts_at?: string;
  ends_at?: string;
  buckets: SendPlanBucket[];
  smart_sending_skips: number | null;
  description: string;
  sampled?: number;
};

export type SendProgressBucket = {
  at: string | null;
  zones: string[];
  waiting: number;
  sent: number;
  skipped_smart_sending: number;
  skipped_other: number;
  failed: number;
};

export type SendProgress = {
  campaign_id: string;
  status: string;
  send_options: SendOptions | null;
  description: string;
  totals: { waiting: number; sent: number; skipped_smart_sending: number; skipped_other: number; failed: number };
  next_release: string | null;
  buckets: SendProgressBucket[];
  skip_reasons: { reason: string; count: number }[];
};

export type SendSettings = {
  sms_window_start: string;
  sms_window_end: string;
  default_sms_window: { start: string; end: string };
  legal_sms_window: { start: string; end: string };
};

import type { DelayUnit, FieldFilter } from "../../api/types";

export type Weekday = "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun";

/** Wait until a date from the triggering event (`event.<property>`) or the profile (`profile.<property>`). */
export type DelayUntil = {
  field: string;
  offset_amount?: number;
  offset_unit?: DelayUnit;
  offset_direction?: "before" | "after";
  /** When the time has already passed. Default: exit for "before", continue for "after". */
  if_past?: "continue" | "exit";
};

/** Optional extras on a delay step, worked out in the person's time zone. */
export type DelayOptions = {
  /** Local time of day, "HH:MM". */
  at?: string | null;
  weekdays?: Weekday[] | null;
  until?: DelayUntil | null;
};

/** Filters on the event that started the run (a trigger split). */
export type EventSplit = { match: "all" | "any"; filters: FieldFilter[] };

export type NotifyNode = {
  id: string;
  type: "notify";
  name?: string;
  to?: string[];
  to_property?: string;
  subject: string;
  body: string;
  allow_browser_events?: boolean;
  next: string | null;
};

export type StepMessages = {
  recipients: number;
  sent: number;
  delivered: number;
  opened: number;
  clicked: number;
  bounced: number;
  unsubscribed: number;
  skipped: number;
  failed: number;
  open_rate: number;
  click_rate: number;
  skip_reasons: { reason: string; count: number }[];
  orders: number;
  revenue: number;
};

export type StepResult = {
  node_id: string;
  type: string;
  reached: number;
  waiting: number;
  messages?: StepMessages;
  notifications?: { sent: number; skipped: number; failed: number; unconfirmed: number };
};

export type FlowResults = {
  flow_id: string;
  range: { start: string; end: string; timezone: string };
  note: string;
  steps: StepResult[];
};

export type WaitingPerson = {
  run_id: string;
  profile_id: string;
  entered_at: string;
  waiting_until: string | null;
  email: string | null;
  phone_number: string | null;
  first_name: string | null;
  last_name: string | null;
};

export type WaitingPage = { data: WaitingPerson[]; next_after: string | null };

export type BackfillStatus = "running" | "done" | "cancelled" | "failed";

export type Backfill = {
  id: string;
  flow_id: string;
  status: BackfillStatus;
  max_people: number;
  total: number | null;
  added: number;
  skipped: number;
  error: string | null;
  created_at: string;
  finished_at: string | null;
};

export type BackfillDryRun = {
  dry_run: true;
  trigger: "list" | "segment" | "date_property" | "metric";
  flow_status: string;
  would_add: number;
  more_than_max: boolean;
  max_people: number;
  running: Backfill | null;
  note: string;
};

export type NotifyAddressStatus = "team" | "confirmed" | "unconfirmed" | "removed" | "new" | "bounced" | "complained";

export type IntegrationStatus = "enabled" | "disabled";

export type SignatureFormat = "plain" | "stripe" | "svix";

export type Signature =
  | { type: "none" }
  | {
      type: "hmac";
      header: string;
      algorithm?: "sha256" | "sha1";
      format?: SignatureFormat;
      encoding?: "hex" | "base64";
      prefix?: string;
      timestamp_header?: string;
      tolerance_seconds?: number;
      secret_encoding?: "base64";
    }
  | { type: "shared_secret"; header?: string; query?: string };

export type Backfill = {
  id: string;
  status: "running" | "done" | "capped" | "cancelled" | "failed";
  since: string;
  started_at: string;
  finished_at?: string;
  pages: number;
  error?: string;
  note?: string;
};

export type IntegrationSummary = {
  id: string;
  name: string;
  recipe: string | null;
  status: IntegrationStatus;
  has_pull: boolean;
  deliveries_ok: number;
  deliveries_failed: number;
  last_delivery_at: string | null;
  last_error: string | null;
  last_error_at: string | null;
  created_at: string;
};

export type Integration = IntegrationSummary & {
  /** Null for people who can see integrations but not manage them (the URL is a credential). */
  inbound_url: string | null;
  /** True when this is the read-only view (no URL, saved state or credential hints). */
  limited?: boolean;
  signature: Signature;
  signature_secret: { saved: boolean; hint?: string };
  signature_ready: boolean;
  script: string;
  settings: Record<string, unknown>;
  pull: Record<string, unknown> | null;
  pull_next_at: string | null;
  /** Saved credential names with a short hint each; values are never returned. */
  credentials?: Record<string, string>;
  missing_credentials: string[];
  oauth_token_cached_until: string | null;
  state?: Record<string, unknown>;
  backfill: Backfill | null;
  /** The metrics the script may record; they can't be sent from a browser. */
  metrics: string[];
  /** Fields removed from payloads before they're stored or shown, like "**.keyCode". */
  redact: string[];
  /** Everyone this integration adds has agreed to get marketing email. */
  contacts_opted_in: boolean;
  /** Where a webhook carries the app's own message id, so resends are ignored. */
  message_id: { header: string } | { field: string } | null;
  updated_at: string;
};

export type IntegrationChanges = {
  name?: string;
  script?: string;
  signature?: Signature;
  signature_secret?: string;
  settings?: Record<string, unknown>;
  pull?: Record<string, unknown> | null;
  credentials?: Record<string, string | null>;
  state?: Record<string, unknown>;
  contacts_opted_in?: boolean;
  metrics?: string[];
  redact?: string[];
};

export type RecipeCredential = { name: string; label: string; help?: string };

export type Recipe = {
  key: string;
  name: string;
  description: string;
  category: string | null;
  credentials: RecipeCredential[];
  signature_secret: { label: string; help: string } | null;
  setup: string[];
  has_pull: boolean;
  flows: { name: string; description: string }[] | null;
  docs: string[] | null;
};

export type FromRecipeResult = {
  integration: Integration;
  flows: { id: string; name: string; status: string }[];
  setup: string[];
};

export type DeliveryStatus = "pending" | "ok" | "error" | "ignored";
export type DeliverySource = "webhook" | "pull" | "test" | "replay";

export type WriteCounts = {
  profiles_created: number;
  profiles_updated: number;
  events: number;
  duplicate_events: number;
  list_adds: number;
  list_removes: number;
  unsubscribes: number;
  suppressed: number;
  consents: number;
  skipped: number;
};

export type Delivery = {
  id: string;
  source: DeliverySource;
  status: DeliveryStatus;
  error: string | null;
  attempts: number;
  next_attempt_at: string | null;
  duration_ms: number | null;
  counts: WriteCounts | null;
  replay_of: string | null;
  created_at: string;
  processed_at: string | null;
  has_payload: boolean;
  replayable: boolean;
};

export type DeliveryDetail = Omit<Delivery, "counts" | "has_payload" | "replayable"> & {
  content_type?: string | null;
  headers?: Record<string, string>;
  query?: Record<string, string>;
  payload?: unknown;
  body?: string | null;
  request?: Record<string, unknown> | null;
  result?: { counts: WriteCounts; effects: string[]; logs: string[] } | null;
  /** Set when the caller may see the log line but not the payload or script output. */
  payload_hidden?: boolean;
  counts?: WriteCounts | null;
};

export type TestResult = {
  ok: boolean;
  error?: string;
  actions?: unknown;
  would_write?: WriteCounts;
  effects?: string[];
  logs?: string[];
  state_after?: Record<string, unknown> | null;
  next_request?: Record<string, unknown> | null;
  pull?: { request: unknown; response: unknown } | null;
  duration_ms: number;
};

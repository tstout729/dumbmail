export type TopicInfo = { topic: string; group: string; description: string };

export type EndpointStatus = "active" | "failing" | "disabled";

export type FilterCondition = { field: string; op: string; value?: unknown };

export type Endpoint = {
  id: string;
  url: string;
  description: string;
  topics: string[];
  metrics: string[];
  filter: FilterCondition[] | null;
  trusted_only: boolean;
  enabled: boolean;
  status: EndpointStatus;
  source: "dashboard" | "api" | "zapier" | "make";
  batch_size: number;
  rate_per_second: number;
  secret_hint: string;
  previous_secret_valid_until: string | null;
  consecutive_failures: number;
  failing_since: string | null;
  next_retry_at: string | null;
  disabled_reason: string | null;
  last_success_at: string | null;
  last_failure_at: string | null;
  created_at: string;
  updated_at: string;
  revision: number;
  stats: { pending: number; delivered_24h: number; failed_24h: number; dropped_total: number; last_dropped_at: string | null };
};

export type CreatedEndpoint = Endpoint & { secret: string };

export type EndpointInput = {
  url?: string;
  description?: string;
  topics?: string[];
  metrics?: string[];
  trusted_only?: boolean;
  enabled?: boolean;
  batch_size?: number;
  rate_per_second?: number;
};

export type DeliveryStatus = "pending" | "sending" | "delivered" | "failed" | "dropped" | "cancelled";

export type Delivery = {
  id: string;
  event_id: string;
  topic: string;
  status: DeliveryStatus;
  test: boolean;
  attempts: number;
  response_code: number | null;
  error: string | null;
  duration_ms: number | null;
  next_attempt_at: string | null;
  last_attempt_at: string | null;
  delivered_at: string | null;
  created_at: string;
};

export type DeliveryDetail = Delivery & { payload: unknown };

export type TestResult = { ok: boolean; delivery: Delivery };

export type RotatedSecret = { secret: string; secret_hint: string; previous_secret_valid_until?: string };

export type FlowSecret = { secret_hint: string | null; created_at: string | null };

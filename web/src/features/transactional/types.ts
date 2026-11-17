export type SendStatus = "queued" | "sent" | "delivered" | "opened" | "clicked" | "bounced" | "complained" | "skipped" | "failed";

export type TransactionalSend = {
  id: string;
  to: string;
  subject: string | null;
  status: SendStatus;
  error: string | null;
  template_id: string | null;
  profile_id: string | null;
  message_id: string | null;
  idempotency_key: string | null;
  metadata: Record<string, unknown>;
  track_clicks: boolean;
  via: string;
  created_at: string;
  sent_at: string | null;
  delivered_at: string | null;
  opened_at: string | null;
  clicked_at: string | null;
  bounce_type: string | null;
};

export type TransactionalSendDetail = TransactionalSend & {
  request: { variables?: Record<string, unknown>; template_id?: string; content?: unknown };
  resent_from: string | null;
};

export type TransactionalTemplate = {
  id: string;
  name: string;
  content: { subject?: string };
  created_at: string;
  updated_at: string;
  review: { flags: string[]; blocked: boolean };
};

export type TransactionalSettings = {
  per_minute_limit: number;
  daily_limit: number;
  sent_last_minute: number;
  sent_last_24h: number;
  paused: boolean;
  paused_reason: string | null;
};

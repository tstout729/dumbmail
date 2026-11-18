/** Push content, as stored on a campaign or a flow push step (see docs/PUSH.md). */
export type PushPlatform = "web" | "ios" | "android";
export type PushUrgency = "very_low" | "low" | "normal" | "high";
export type PushAction = { title: string; url: string };

export type PushContent = {
  title: string;
  body: string;
  url?: string;
  icon?: string;
  image?: string;
  actions?: PushAction[];
  /** Empty or missing: every platform. */
  platforms?: PushPlatform[];
  ttl_seconds?: number;
  urgency?: PushUrgency;
  topic?: string;
  ios_sound?: string;
  ios_badge?: number;
  android_channel_id?: string;
};

export type PushRender = {
  title: string;
  body: string;
  url: string;
  icon: string | null;
  image: string | null;
  actions: PushAction[];
  platforms: PushPlatform[];
};

export type PushPrompt = {
  enabled: boolean;
  title: string;
  body: string;
  allow_text: string;
  deny_text: string;
  page_views: number;
  delay_seconds: number;
  url_contains: string[];
  url_excludes: string[];
  frequency_days: number;
  position: "top" | "bottom";
};

export type PushSettings = {
  web: { enabled: boolean; ready: boolean; vapid_public_key: string | null; sw_path: string; sw_scope: string; prompt: PushPrompt; service_worker_url: string; handler_url: string };
  apns: { configured: boolean; key_id: string | null; team_id: string | null; bundle_id: string | null; production: boolean; verified: boolean; error: string | null };
  fcm: { configured: boolean; project_id: string | null; client_email: string | null; error: string | null };
  identity: { configured: boolean; hint: string | null };
  account_id: string;
  public_key: string;
  register_url: string;
  devices: { web: number; ios: number; android: number; anonymous: number; permission_denied: number };
  reachable_people: number;
  opted_out: number;
};

export type ApnsInput = { key_id: string; team_id: string; bundle_id: string; production: boolean; key_p8?: string };

export type PushSettingsPatch = {
  web_enabled?: boolean;
  sw_path?: string;
  sw_scope?: string;
  prompt?: PushPrompt;
  apns?: ApnsInput | null;
  fcm?: { service_account_json: string } | null;
};

export type PushTestResult = { message_id: string; devices: { kind: PushPlatform; status: string; error: string | null }[] };

export type PushReport = {
  people: { sent: number; clicked: number; failed: number; skipped: number; retrying: number; unconfirmed: number };
  platforms: { platform: PushPlatform; sent: number; delivered: number | null; clicked: number; failed: number; removed: number; retrying: number; unconfirmed: number }[];
};

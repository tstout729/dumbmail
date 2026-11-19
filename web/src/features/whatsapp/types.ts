/** WhatsApp content for a campaign or flow step: an approved template and what fills its variables. */
export type WhatsappContent = {
  template_id: string | null;
  header?: string;
  header_media_url?: string;
  /** Each body variable by name ("1", "2" … or a named variable) to its value. */
  body: Record<string, string>;
  /** Link buttons whose URL ends in a variable, by button position. */
  buttons?: Record<string, string>;
};

export type WhatsappTestNumber = { id: string; phone_number: string; member: string; verified: boolean; verified_at: string | null; created_at: string };

export type WhatsappSettings = {
  connected: boolean;
  /** What's still needed before sending turns on. */
  missing: string[];
  connected_via: string;
  waba_id: string | null;
  phone_number_id: string | null;
  access_token_hint: string | null;
  access_token_set: boolean;
  app_secret_set: boolean;
  webhook_url: string;
  verify_token: string;
  webhook_working: boolean;
  last_webhook_at: string | null;
  phone: {
    display_phone_number: string | null;
    verified_name: string | null;
    quality_rating: string | null;
    messaging_limit: string | null;
    checked_at: string | null;
  };
  error: string | null;
  error_at: string | null;
  test_numbers: WhatsappTestNumber[];
  signup_keywords: string[];
  signup_reply: string | null;
  optin_buttons: string[];
  templates: { total: number; approved: number };
  embedded_signup_available: boolean;
};

export type WhatsappSettingsPatch = {
  waba_id?: string;
  phone_number_id?: string;
  access_token?: string;
  app_secret?: string;
  signup_keywords?: string[];
  signup_reply?: string;
  optin_buttons?: string[];
};

export type TemplateComponent = {
  type: string;
  format?: string;
  text?: string;
  buttons?: { type: string; text: string; url?: string }[];
};

export type WhatsappTemplate = {
  id: string;
  meta_id: string;
  name: string;
  language: string;
  category: string;
  status: string;
  parameter_format: string;
  components: TemplateComponent[];
  reason: string | null;
  quality: string | null;
  created_by_dumbmail: boolean;
  synced_at: string;
  tracked_buttons: Record<string, string>;
  variables: { header: string | null; header_media: string | null; body: string[]; url_buttons: number[] };
  sendable: boolean;
};

export type NewTemplateButton = { type: "quick_reply" | "url"; text: string; url?: string; track?: boolean };

export type NewTemplate = {
  name: string;
  language: string;
  category: "MARKETING" | "UTILITY";
  header?: string;
  header_example?: string;
  body: string;
  body_examples: string[];
  footer?: string;
  buttons: NewTemplateButton[];
};

export type WhatsappPreview = {
  channel: "whatsapp";
  template?: { id: string; name: string; language: string; category: string; status: string };
  header_media?: string | null;
  header_media_url?: string | null;
  text: string;
  buttons?: { type: string; text: string; url: string | null; tracked: boolean }[];
  problems: string[];
};

export type WhatsappReport = {
  sent: number;
  delivered: number;
  read: number;
  clicked: number;
  failed: number;
  skipped: number;
  messages: number;
  reasons: { reason: string | null; count: number }[];
};

export type WhatsappConversation = {
  id: string;
  channel: "whatsapp";
  profile_id: string;
  phone_number: string;
  name: string | null;
  status: "open" | "closed";
  unread_count: number;
  last_message_at: string;
  last_direction: "inbound" | "outbound";
  last_preview: string;
  window_open: boolean;
  whatsapp_consent: string;
};

export type WhatsappMessage = {
  id: string;
  direction: "inbound" | "outbound";
  body: string;
  kind: string;
  at: string;
  type?: string;
  keyword?: string | null;
  template?: string | null;
  status?: string;
  error?: string | null;
  campaign_id?: string | null;
  flow_id?: string | null;
};

export type WhatsappContact = {
  phone_number: string;
  consent: "subscribed" | "unsubscribed" | "never_subscribed";
  source: string | null;
  changed_at: string | null;
  on_whatsapp: boolean;
  window_open_until: string | null;
  history: { consent: string; source: string; at: string }[];
};

export type WhatsappConversationDetail = {
  id: string;
  channel: "whatsapp";
  profile_id: string;
  phone_number: string;
  name: string | null;
  status: "open" | "closed";
  messages: WhatsappMessage[];
  contact: WhatsappContact;
  reply: { free_form: boolean; why: string | null; templates: WhatsappTemplate[] };
};

export type WhatsappReplyResult = { dry_run?: boolean; sent: boolean; message_id?: string; text?: string; reason?: string };

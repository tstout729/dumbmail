import type { Consent } from "../../api";

export type ConversationStatus = "open" | "closed";

export type ConversationProfile = {
  id: string;
  first_name: string | null;
  last_name: string | null;
  phone_number: string | null;
  email: string | null;
  sms_consent: Consent;
};

export type Conversation = {
  id: string;
  status: ConversationStatus;
  unread_count: number;
  last_message_at: string;
  last_inbound_at: string | null;
  last_direction: "inbound" | "outbound";
  last_preview: string;
  can_reply: boolean;
  profile: ConversationProfile;
};

export type ConversationList = { data: Conversation[]; total: number };

/** inbound: message | stop | start | help | keyword | confirm. outbound: marketing | reply | auto_reply. */
export type ThreadMessageKind = "message" | "stop" | "start" | "help" | "keyword" | "confirm" | "marketing" | "reply" | "auto_reply";

export type ThreadMedia = { url: string; content_type: string | null; stored: boolean };

export type ThreadMessage = {
  id: string;
  direction: "inbound" | "outbound";
  kind: ThreadMessageKind;
  keyword: string | null;
  /** Null for texts sent before the inbox kept their words. */
  body: string | null;
  media: ThreadMedia[];
  at: string;
  status: string | null;
  error: string | null;
  campaign: { id: string; name: string | null } | null;
  flow: { id: string; name: string | null } | null;
  sent_by: string | null;
};

export type ConversationDetail = {
  conversation: Conversation;
  messages: ThreadMessage[];
  has_more: boolean;
  reply: { allowed: boolean; reason: string | null; window_ends_at: string | null };
};

export type ReplyDryRun = { dry_run: true; to: string; body: string; segments: number; note: string };
export type ReplySent = { sent: true; message_id: string };

export type KeywordMatch = "exact" | "starts_with";

export type SmsKeyword = {
  id: string;
  keyword: string;
  aliases: string[];
  match_type: KeywordMatch;
  subscribe: boolean;
  double_opt_in: boolean;
  list_id: string | null;
  reply: string;
  enabled: boolean;
  created_at: string;
  updated_at: string;
};

export type SmsKeywordInput = Partial<Omit<SmsKeyword, "id" | "created_at" | "updated_at">>;

export type SavedReply = { id: string; name: string; body: string; created_at: string; updated_at: string };

export type InboxMember = { id: string; email: string; name: string | null };

export type InboxSettings = {
  program_name: string;
  message_frequency: string;
  default_reply_enabled: boolean;
  default_reply: string;
  away_reply_enabled: boolean;
  away_reply: string;
  open_hour: number;
  close_hour: number;
  /** ISO weekdays: 1 = Monday … 7 = Sunday. */
  open_days: number[];
  notify_user_ids: string[];
  members: InboxMember[];
  disclosure_ready: boolean;
};

export type InboxSettingsPatch = Partial<Omit<InboxSettings, "members" | "disclosure_ready">>;

export type Unread = { conversations: number; texts: number };

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
  /** Their number opted out (texted STOP): nothing can be sent to it until they text START. */
  opted_out: boolean;
  /** The number that texted, and the one of yours they texted (replies go between these). */
  their_number: string | null;
  our_number: string | null;
  profile: ConversationProfile;
};

/** One page; pass `next_cursor` as `cursor` for the next (null on the last page). */
export type ConversationList = { data: Conversation[]; total: number; next_cursor: string | null };

/**
 * inbound: message | stop | start | help | keyword | confirm | blocked (texted while opted out).
 * outbound: marketing | reply | auto_reply | keyword_reply | confirmation | optin_request | help.
 */
export type ThreadMessageKind =
  | "message"
  | "stop"
  | "start"
  | "help"
  | "keyword"
  | "confirm"
  | "blocked"
  | "marketing"
  | "reply"
  | "auto_reply"
  | "keyword_reply"
  | "confirmation"
  | "optin_request";

/** `url` is null for a provider link that can't be opened from a browser (Twilio's need the account's credentials). */
export type ThreadMedia = { url: string | null; content_type: string | null; stored: boolean };

export type ThreadMessage = {
  id: string;
  direction: "inbound" | "outbound";
  kind: ThreadMessageKind;
  keyword: string | null;
  /** Null for texts sent before the inbox kept their words. */
  body: string | null;
  media: ThreadMedia[];
  /** Inbound only: the provider's signature checked out. */
  verified: boolean | null;
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
  /** Pass as `cursor` for the next older page. */
  next_cursor: string | null;
  reply: { allowed: boolean; reason: string | null; window_ends_at: string | null; to: string | null; from: string | null };
};

export type ReplyDryRun = { dry_run: true; to: string; from: string; body: string; segments: number; note: string };
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
  version: number;
  created_at: string;
  updated_at: string;
};

export type SmsKeywordInput = Partial<Omit<SmsKeyword, "id" | "version" | "created_at" | "updated_at">>;

export type SavedReply = { id: string; name: string; body: string; created_at: string; updated_at: string };

export type InboxMember = { id: string; email: string; name: string | null };

export type HelpReplies = "auto" | "dumbmail" | "provider";

export type InboxVerification = { configured: boolean; provider: "telnyx" | "twilio" | "log"; how: string };

export type InboxSettings = {
  program_name: string;
  message_frequency: string;
  support_contact: string;
  help_replies: HelpReplies;
  auto_budget_account: number;
  auto_budget_number: number;
  auto_budget_hit_at: string | null;
  /** "account", or the number whose budget ran out. */
  auto_budget_hit_scope: string | null;
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
  support_ready: boolean;
  verification: InboxVerification;
  /** The start of your own Telnyx account's public key, when saved. */
  telnyx_public_key_hint: string | null;
  max_budget_account: number;
  max_budget_number: number;
  /** A budget ran out this hour: automatic texts are paused until the next hour. */
  auto_paused: boolean;
};

type ReadOnlySettings = "members" | "disclosure_ready" | "support_ready" | "verification" | "telnyx_public_key_hint" | "max_budget_account" | "max_budget_number" | "auto_paused" | "auto_budget_hit_at" | "auto_budget_hit_scope";

export type InboxSettingsPatch = Partial<Omit<InboxSettings, ReadOnlySettings>> & { telnyx_public_key?: string | null };

export type Unread = { conversations: number; texts: number };

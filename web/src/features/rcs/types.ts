// Mirrors docs/RCS.md and src/features/rcs.

export type RcsSuggestion = { type: "reply" | "url"; text: string; url?: string };
export type RcsCard = { image_url: string; title: string };
/** The optional rich version of a text (`content.rcs`). */
export type RcsRich = { card?: RcsCard; suggestions?: RcsSuggestion[] };
/** SMS content with its rich version. */
export type SmsWithRich = { body: string; media_url?: string; rcs?: RcsRich };

/** The rich version as the server preview shows it (`/api/render` → `rcs`). */
export type RcsPreview = { card: { image_url: string | null; title: string | null } | null; suggestions: RcsSuggestion[] };

export type RcsSettings = {
  available: boolean;
  unavailable_reason: string | null;
  provider: "platform" | "own_telnyx";
  agent_id: string | null;
  agent_name: string | null;
  confirmed: boolean;
  checked_at: string | null;
  check_problem: string | null;
  enabled: boolean;
  sending: boolean;
  needs_check: boolean;
  signing_key_ready: boolean;
  webhook_url: string;
  price_per_message: number;
  price_per_sms_segment: number;
};

export type RcsSettingsPatch = { agent_id?: string | null; enabled?: boolean };

export type RcsSide = { sent: number; delivered: number; clicked: number; read?: number };
export type RcsReport = { rcs: RcsSide; sms: RcsSide; any_rcs: boolean };

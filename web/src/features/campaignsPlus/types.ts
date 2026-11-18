import type { Campaign, CampaignStatus, Channel, MessageStats } from "../../api";

export type TagRef = { id: string; name: string };
export type TagSummary = TagRef & { campaigns: number; flows: number; templates: number };
export type TagList = { data: TagSummary[]; limits: { per_item: number; per_account: number; name: number } };

/** A campaign in the list, with what this feature adds. */
export type CampaignListItem = Campaign & { tags: TagRef[]; archived_at: string | null; resend_of: string | null };

export type CampaignSearch = {
  status?: string;
  channel?: Channel;
  tag?: string;
  q?: string;
  from?: string;
  to?: string;
  archived?: "false" | "true" | "all";
  limit?: number;
  offset?: number;
};

export type CampaignCounts = { by_status: Partial<Record<CampaignStatus, number>>; archived: number };

export type ArchiveResult = { changed: string[]; skipped: { id: string; reason: string }[] };

export type LinkParams = {
  source?: string | null;
  medium?: string | null;
  campaign?: string | null;
  content?: string | null;
  term?: string | null;
  custom: { name: string; value: string }[];
};

export type LinkVariable = { name: string; description: string };

export type LinkDefaults = {
  enabled: boolean;
  params: LinkParams;
  skip_domains: string[];
  fallbacks: { source: string; medium: string; campaign: string };
  variables: LinkVariable[];
};

export type LinkOverride = { mode: "default" } | { mode: "off" } | { mode: "custom"; params: LinkParams };

export type LinkTags = {
  override: LinkOverride;
  account_enabled: boolean;
  tagged: boolean;
  params: { name: string; value: string }[];
  skip_domains: string[];
  preview: string | null;
  variables: LinkVariable[];
  note?: string;
};

export type ResendTarget = "no_open" | "no_click";

export type ResendInfo = {
  resend_of: { id: string; name: string; target: ResendTarget; number: number } | null;
  audience_note?: string;
  undercount_note: string;
  resends?: { id: string; name: string; status: CampaignStatus; target: ResendTarget; number: number; send_at: string | null; sent_at: string | null; stats: MessageStats }[];
  combined?: { people_reached: number; people_opened: number; people_clicked: number; emails_delivered: number; open_rate: number; click_rate: number } | null;
  can_resend?: { ok: boolean; reason: string | null; earliest_at: string | null; latest_at: string | null; resends_used: number; resends_allowed: number };
};

export type CalendarItem = {
  id: string;
  name: string;
  channel: Channel;
  status: CampaignStatus;
  at: string;
  date: string;
  time: string;
  timing: "all_at_once" | "local_time" | "best_time" | "gradual";
  note: string | null;
  archived: boolean;
  resend: boolean;
  ab_test: boolean;
  can_move: boolean;
};

export type CalendarData = { from: string; to: string; timezone: string; items: CalendarItem[]; truncated: boolean };

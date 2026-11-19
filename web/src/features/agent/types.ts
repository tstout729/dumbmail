export type AgentAnswer = { question: string; answer: string };

export type AgentPageStatus = "pending" | "ready" | "failed";

export type AgentPage = {
  id: string;
  url: string;
  title: string | null;
  status: AgentPageStatus;
  error: string | null;
  fetched_at: string | null;
};

export type AgentSettings = {
  hub: boolean;
  site: boolean;
  sms: boolean;
  whatsapp: boolean;
  name: string;
  tone: string;
  answers: AgentAnswer[];
  monthly_cap: number;
  display_name: string;
  pages: AgentPage[];
  available: boolean;
  model: string;
  conversations_this_month: number;
  price_per_conversation_usd: number;
  hub_enabled: boolean;
  site_origins: string[];
  website: string | null;
};

export type AgentSettingsPatch = Partial<Pick<AgentSettings, "hub" | "site" | "sms" | "whatsapp" | "name" | "tone" | "answers" | "monthly_cap">> & { pages?: string[] };

export type AgentPlace = "hub" | "site" | "sms" | "whatsapp" | "test";

export type AgentConversation = {
  id: string;
  place: AgentPlace;
  status: "open" | "handed_off" | "ended";
  handed_off: boolean;
  handoff_reason: string | null;
  handoff_summary: string | null;
  turns: number;
  created_at: string;
  last_message_at: string;
  person: { id: string; email: string | null; phone_number: string | null; name: string | null } | null;
  last_message: string | null;
  sms_conversation_id: string | null;
  whatsapp_conversation_id: string | null;
};

export type AgentConversationList = { data: AgentConversation[]; next_cursor: string | null; handed_off_count: number };

export type AgentMessage = { role: "shopper" | "agent" | "note"; body: string; at: string };

export type AgentConversationDetail = AgentConversation & { messages: AgentMessage[]; older_cursor: string | null };

export type AgentTestReply = { reply: string | null; conversation: string | null; conversation_id: string | null; handed_off: boolean; refused: string | null };

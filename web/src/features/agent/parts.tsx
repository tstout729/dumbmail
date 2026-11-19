import { Badge } from "../../components/ui";
import type { AgentConversation, AgentPlace } from "./types";

export const PLACE_LABEL: Record<AgentPlace, string> = { hub: "Customer hub", site: "Website", sms: "Text", whatsapp: "WhatsApp", test: "Test chat" };

export const REASON_LABEL: Record<string, string> = {
  refund: "Refund",
  complaint: "Complaint",
  legal_or_medical: "Legal or medical",
  frustrated: "Frustrated shopper",
  asked_for_person: "Asked for a person",
  unsure: "Agent wasn't sure",
  agent_stopped: "Agent couldn't answer",
  limit: "Chat got too long",
  other: "Other",
};

export function StatusLabel({ c }: { c: AgentConversation }) {
  if (c.handed_off) return <Badge tone="amber">Handed off</Badge>;
  return <Badge tone="gray">Answered</Badge>;
}

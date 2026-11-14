import { Mail, MessageSquare } from "lucide-react";
import type { FlowBlueprint } from "../../api";

export function blueprintChannels(b: FlowBlueprint): string[] {
  const mix = Array.isArray(b.channel_mix) ? b.channel_mix : String(b.channel_mix).split(/[\s,+/&]+/);
  return mix.map((c) => c.toLowerCase()).filter((c) => c === "email" || c === "sms");
}

export function ChannelIcons({ channels }: { channels: string[] }) {
  return (
    <span className="flex items-center gap-1 text-zinc-400">
      {channels.includes("email") ? <Mail className="size-3.5" aria-label="Email" /> : null}
      {channels.includes("sms") ? <MessageSquare className="size-3.5" aria-label="SMS" /> : null}
    </span>
  );
}

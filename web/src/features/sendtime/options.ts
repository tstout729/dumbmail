import type { Campaign } from "../../api";
import type { SendOptions, Timing, WithSendOptions } from "./types";

export const defaultLocalTime = "10:00";

/** The campaign's send options, or null when it goes to everyone at once. */
export function sendOptionsOf(c: Campaign): SendOptions | null {
  return (c as Campaign & WithSendOptions).send_options ?? null;
}

/** Switch to a timing mode, keeping smart sending and filling that mode's defaults. */
export function withTiming(o: SendOptions | null, timing: Timing): SendOptions | null {
  const smart = { smart_sending: o?.smart_sending ?? false, smart_sending_hours: o?.smart_sending ? (o.smart_sending_hours ?? 16) : undefined };
  switch (timing) {
    case "all_at_once":
      return smart.smart_sending ? { timing, ...smart } : null;
    case "local_time":
      return { timing, local_time: o?.local_time ?? defaultLocalTime, late: o?.late ?? "next_day", ...smart };
    case "best_time":
      return { timing, window_start: o?.window_start ?? "08:00", window_end: o?.window_end ?? "20:00", within_hours: o?.within_hours ?? 24, ...smart };
    case "gradual":
      return { timing, spread_hours: o?.spread_hours ?? 4, ...smart };
  }
}

export function withSmartSending(o: SendOptions | null, on: boolean, hours?: number): SendOptions | null {
  const base: SendOptions = o ?? { timing: "all_at_once", smart_sending: false };
  const next: SendOptions = { ...base, smart_sending: on, smart_sending_hours: on ? (hours ?? base.smart_sending_hours ?? 16) : undefined };
  return next.timing === "all_at_once" && !on ? null : next;
}

/** Options as they go to the server: the day is picked when it's scheduled. */
export function forPreview(o: SendOptions | null): SendOptions | null {
  if (!o) return null;
  const rest: SendOptions = { ...o };
  delete rest.local_date;
  return rest;
}

export function timingLabel(o: SendOptions | null): string {
  switch (o?.timing ?? "all_at_once") {
    case "local_time":
      return `${o?.local_time ?? defaultLocalTime} in each person's time zone`;
    case "best_time":
      return "Best time for each person";
    case "gradual":
      return `Spread over ${o?.spread_hours ?? 4} hours`;
    default:
      return "Everyone at once";
  }
}

/** "10:00" → "10:00 AM". */
export function clockLabel(hhmm: string): string {
  const [h, m] = hhmm.split(":").map(Number);
  const d = new Date(Date.UTC(2000, 0, 1, h, m));
  return new Intl.DateTimeFormat("en-US", { hour: "numeric", minute: "2-digit", timeZone: "UTC" }).format(d);
}

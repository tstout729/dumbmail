import { Badge, type Tone } from "../../components/ui";
import type { DeliveryStatus, IntegrationStatus, WriteCounts } from "./types";

export function IntegrationStatusBadge({ status, error }: { status: IntegrationStatus; error?: string | null }) {
  if (status === "disabled") return <Badge dot>Off</Badge>;
  return error ? (
    <Badge tone="amber" dot>
      On, with errors
    </Badge>
  ) : (
    <Badge tone="green" dot>
      On
    </Badge>
  );
}

const deliveryTones: Record<DeliveryStatus, Tone> = { ok: "green", error: "red", ignored: "gray", pending: "blue" };
const deliveryLabels: Record<DeliveryStatus, string> = { ok: "Done", error: "Failed", ignored: "Nothing to do", pending: "Waiting" };

export function DeliveryStatusBadge({ status, retrying }: { status: DeliveryStatus; retrying?: boolean }) {
  return (
    <Badge tone={retrying ? "amber" : deliveryTones[status]} dot>
      {retrying ? "Failed, will retry" : deliveryLabels[status]}
    </Badge>
  );
}

/** "2 profiles, 3 events" from a delivery's counts. */
export function countsSummary(c: WriteCounts | null | undefined): string {
  if (!c) return "";
  const parts: string[] = [];
  const add = (n: number, one: string, many: string) => {
    if (n > 0) parts.push(`${n} ${n === 1 ? one : many}`);
  };
  add(c.profiles_created, "new profile", "new profiles");
  add(c.profiles_updated, "profile updated", "profiles updated");
  add(c.events, "event", "events");
  add(c.duplicate_events, "repeat skipped", "repeats skipped");
  add(c.list_adds + c.list_removes, "list change", "list changes");
  add(c.consents, "consent change", "consent changes");
  add(c.unsubscribes, "unsubscribe", "unsubscribes");
  add(c.suppressed, "suppressed", "suppressed");
  add(c.skipped, "skipped", "skipped");
  return parts.join(", ");
}

export function JsonBlock({ value, className }: { value: unknown; className?: string }) {
  const text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  return <pre className={`max-h-96 overflow-auto rounded-md border border-zinc-200 bg-zinc-50 p-3 font-mono text-xs leading-relaxed text-zinc-800 ${className ?? ""}`}>{text}</pre>;
}

/** Parse JSON typed by a person; returns the value or a readable error. */
export function parseJson(text: string): { ok: true; value: unknown } | { ok: false; error: string } {
  try {
    return { ok: true, value: JSON.parse(text) as unknown };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? `Not valid JSON: ${e.message}` : "Not valid JSON" };
  }
}

export const monoTextarea = "font-mono text-xs leading-relaxed";

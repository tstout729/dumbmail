import { Star } from "lucide-react";
import { Badge } from "../../components/ui";
import type { ReviewStatus } from "./types";

/** Five stars, filled up to `value` (rounded to the nearest half). */
export function Stars({ value, size = "sm" }: { value: number | null; size?: "sm" | "md" }) {
  const v = Math.round((value ?? 0) * 2) / 2;
  const cls = size === "md" ? "size-4" : "size-3.5";
  return (
    <span className="inline-flex items-center gap-0.5" role="img" aria-label={value == null ? "No rating" : `${value.toFixed(1)} out of 5 stars`}>
      {[1, 2, 3, 4, 5].map((n) => (
        <Star key={n} aria-hidden className={`${cls} ${n <= v ? "fill-amber-400 text-amber-400" : n - 0.5 === v ? "fill-amber-200 text-amber-400" : "text-zinc-300"}`} />
      ))}
    </span>
  );
}

const statusLabel: Record<ReviewStatus, string> = { pending: "Waiting", published: "Published", rejected: "Rejected", removed: "Deleted by author" };
const statusTone = { pending: "amber", published: "green", rejected: "red", removed: "gray" } as const;

export function ReviewStatusBadge({ status, awaitingConfirmation }: { status: ReviewStatus; awaitingConfirmation?: boolean }) {
  if (awaitingConfirmation) return <Badge tone="gray">Email not confirmed</Badge>;
  return <Badge tone={statusTone[status]}>{statusLabel[status]}</Badge>;
}

export const rejectReasonLabels = {
  spam: "Spam",
  offensive: "Offensive or abusive",
  personal_info: "Shares personal details",
  not_about_this: "Not about this product",
  fake: "Fake or not a real customer",
  other: "Other",
} as const;

export function flagLabel(flag: string): string {
  if (flag.startsWith("word:")) return `Word: ${flag.slice(5)}`;
  return { profanity: "Profanity", email: "Email address", phone: "Phone number", link: "Link" }[flag] ?? flag;
}

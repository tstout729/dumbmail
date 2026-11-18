import type { Condition } from "../../api";
import { Input, Select } from "../../components/ui";
import type { ReviewCondition } from "./types";

const small = "h-8 w-auto text-xs";

export function newReviewCondition(): ReviewCondition {
  return { type: "review", action: "reviewed", op: "has" };
}

export function isReviewConditionComplete(c: ReviewCondition): boolean {
  const okDays = c.within_days === undefined || (c.within_days >= 1 && c.within_days <= 3650);
  const okRange = c.min_rating === undefined || c.max_rating === undefined || c.min_rating <= c.max_rating;
  return okDays && okRange && (c.product_id === undefined || c.product_id.trim() !== "");
}

export function reviewConditionSummary(c: ReviewCondition): string {
  const not = c.op === "has_not" ? "not " : "";
  const what = c.product_id ? ` ${c.product_id}` : "";
  if (c.action === "pending_request") return `${not}has a review link waiting${what ? ` for${what}` : ""}`;
  const stars = c.min_rating && c.max_rating ? ` ${c.min_rating}-${c.max_rating}★` : c.min_rating ? ` ${c.min_rating}★+` : c.max_rating ? ` up to ${c.max_rating}★` : "";
  const count = (c.count ?? 1) > 1 ? ` at least ${c.count}×` : "";
  return `${not}reviewed${what}${stars}${count}${c.within_days ? ` in the last ${c.within_days}d` : ""}`;
}

/** Segment rule fields for reviews. */
export function ReviewConditionFields({ condition: c, onChange }: { condition: ReviewCondition; onChange: (c: Condition) => void }) {
  const stars = (v: string) => (v === "" ? undefined : Number(v));
  return (
    <>
      <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as ReviewCondition["op"] })} className={small} aria-label="Has or has not">
        <option value="has">has</option>
        <option value="has_not">has not</option>
      </Select>
      <Select
        value={c.action}
        onChange={(e) => {
          const action = e.target.value as ReviewCondition["action"];
          onChange(action === "pending_request" ? { type: "review", action, op: c.op, product_id: c.product_id, within_days: c.within_days } : { ...c, action });
        }}
        className={small}
        aria-label="Reviewed or has a link waiting"
      >
        <option value="reviewed">written a review</option>
        <option value="pending_request">a review link waiting</option>
      </Select>
      <Input value={c.product_id ?? ""} onChange={(e) => onChange({ ...c, product_id: e.target.value || undefined })} placeholder="any product" className="h-8 w-32 text-xs" aria-label="Product id" />
      {c.action === "reviewed" ? (
        <>
          <Select value={c.min_rating ?? ""} onChange={(e) => onChange({ ...c, min_rating: stars(e.target.value) })} className={small} aria-label="Lowest rating">
            <option value="">any rating</option>
            {[1, 2, 3, 4, 5].map((n) => (
              <option key={n} value={n}>
                from {n}★
              </option>
            ))}
          </Select>
          <Select value={c.max_rating ?? ""} onChange={(e) => onChange({ ...c, max_rating: stars(e.target.value) })} className={small} aria-label="Highest rating">
            <option value="">to 5★</option>
            {[1, 2, 3, 4].map((n) => (
              <option key={n} value={n}>
                to {n}★
              </option>
            ))}
          </Select>
          <span className="text-xs text-zinc-500">at least</span>
          <Input type="number" min={1} max={10000} value={c.count ?? 1} onChange={(e) => onChange({ ...c, count: Math.max(1, Number(e.target.value) || 1) })} className="h-8 w-16 text-xs" aria-label="How many reviews" />
          <span className="text-xs text-zinc-500">times</span>
        </>
      ) : null}
      <Select value={c.within_days === undefined ? "all" : "within"} onChange={(e) => onChange({ ...c, within_days: e.target.value === "all" ? undefined : 90 })} className={small} aria-label="Time window">
        <option value="all">ever</option>
        <option value="within">in the last</option>
      </Select>
      {c.within_days !== undefined ? (
        <>
          <Input type="number" min={1} max={3650} value={c.within_days} onChange={(e) => onChange({ ...c, within_days: Math.max(1, Math.min(3650, Number(e.target.value) || 1)) })} className="h-8 w-20 text-xs" aria-label="Days" />
          <span className="text-xs text-zinc-500">days</span>
        </>
      ) : null}
    </>
  );
}

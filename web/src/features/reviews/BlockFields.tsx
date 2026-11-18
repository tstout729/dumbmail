import type { EmailBlock } from "../../api";
import { Checkbox, Field, Input, Select } from "../../components/ui";
import type { ReviewRequestBlock, ReviewsBlock } from "./types";

export function newReviewsBlock(): ReviewsBlock {
  return { type: "reviews", count: 3, show: "both" };
}

export function newReviewRequestBlock(): ReviewRequestBlock {
  return { type: "review_request", heading: "", button_text: "Write a review", show_stars: true };
}

export function reviewsBlockSummary(b: ReviewsBlock): string {
  const what = b.show === "summary" ? "Star summary" : `${b.count ?? 3} top reviews`;
  return `${what} of ${b.product_id ? b.product_id : "the event's product"}`;
}

export function reviewRequestBlockSummary(b: ReviewRequestBlock): string {
  return b.product_id ? `Ask about ${b.product_id}` : "Ask about the items in the event";
}

export function ReviewsBlockFields({ block, onChange }: { block: ReviewsBlock; onChange: (b: EmailBlock) => void }) {
  return (
    <div className="space-y-3">
      <Field label="Product id" hint="Leave empty to use the first item of the event that started the flow. Only published reviews show; with none, the block is left out.">
        <Input value={block.product_id ?? ""} onChange={(e) => onChange({ ...block, product_id: e.target.value || undefined })} placeholder="The event's product" />
      </Field>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Show">
          <Select value={block.show ?? "both"} onChange={(e) => onChange({ ...block, show: e.target.value as ReviewsBlock["show"] })}>
            <option value="both">Stars and reviews</option>
            <option value="summary">Stars only</option>
            <option value="reviews">Reviews only</option>
          </Select>
        </Field>
        <Field label="Reviews">
          <Select value={block.count ?? 3} onChange={(e) => onChange({ ...block, count: Number(e.target.value) })} disabled={block.show === "summary"}>
            {[1, 2, 3, 4, 5].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </Select>
        </Field>
      </div>
    </div>
  );
}

export function ReviewRequestBlockFields({ block, onChange }: { block: ReviewRequestBlock; onChange: (b: EmailBlock) => void }) {
  return (
    <div className="space-y-3">
      <p className="text-xs text-zinc-500">Each person gets their own review link for each item, with stars that open the form. It only shows to people subscribed to email. Previews show a sample link.</p>
      <Field label="Product id" hint="Leave empty to ask about the items in the event that started the flow.">
        <Input value={block.product_id ?? ""} onChange={(e) => onChange({ ...block, product_id: e.target.value || undefined })} placeholder="The event's items" />
      </Field>
      <Field label="Heading">
        <Input value={block.heading ?? ""} maxLength={100} onChange={(e) => onChange({ ...block, heading: e.target.value })} placeholder="Optional" />
      </Field>
      <Field label="Button text">
        <Input value={block.button_text ?? ""} maxLength={60} onChange={(e) => onChange({ ...block, button_text: e.target.value })} placeholder="Write a review" />
      </Field>
      <Checkbox checked={block.show_stars ?? true} onChange={(e) => onChange({ ...block, show_stars: e.target.checked })} label="Show stars" description="Each star opens the form with that rating chosen." />
    </div>
  );
}

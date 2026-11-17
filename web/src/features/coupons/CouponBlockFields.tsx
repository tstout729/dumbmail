import { Checkbox, Field, Input, Select } from "../../components/ui";
import type { EmailBlock } from "../../api";
import { useCoupons } from "./api";
import type { CouponBlock } from "./types";

export function newCouponBlock(): CouponBlock {
  return { type: "coupon", coupon: "", title: "Your code", show_expiry: true, button_text: "Shop now", href: "https://" };
}

export function couponBlockSummary(b: CouponBlock): string {
  return b.coupon ? `Code from ${b.coupon}` : "Pick a coupon";
}

/** The coupon block's fields: which coupon, the heading over the code, the expiry line and a button. */
export function CouponBlockFields({ block, onChange }: { block: CouponBlock; onChange: (b: EmailBlock) => void }) {
  const coupons = useCoupons();
  const list = coupons.data?.data ?? [];
  const missing = block.coupon !== "" && coupons.isSuccess && !list.some((c) => c.name.toLowerCase() === block.coupon.toLowerCase());
  return (
    <div className="space-y-3">
      <Field
        label="Coupon"
        error={missing ? "There's no coupon with this name anymore." : null}
        hint={list.length === 0 && coupons.isSuccess ? "Create a coupon under Content → Coupons first." : "Each person sees their own code. Previews show SAMPLE-CODE."}
      >
        <Select value={block.coupon} onChange={(e) => onChange({ ...block, coupon: e.target.value })} aria-label="Coupon">
          <option value="">Choose a coupon…</option>
          {missing ? <option value={block.coupon}>{block.coupon}</option> : null}
          {list.map((c) => (
            <option key={c.id} value={c.name}>
              {c.name}
            </option>
          ))}
        </Select>
      </Field>
      <Field label="Heading over the code">
        <Input value={block.title ?? ""} maxLength={100} onChange={(e) => onChange({ ...block, title: e.target.value })} placeholder="Your code" />
      </Field>
      <Checkbox checked={block.show_expiry ?? true} onChange={(e) => onChange({ ...block, show_expiry: e.target.checked })} label="Show when it expires" description="Only for coupons with an expiry." />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Button text" hint="Leave empty for no button.">
          <Input value={block.button_text ?? ""} maxLength={60} onChange={(e) => onChange({ ...block, button_text: e.target.value })} placeholder="Shop now" />
        </Field>
        <Field label="Button link">
          <Input type="url" value={block.href ?? ""} onChange={(e) => onChange({ ...block, href: e.target.value })} placeholder="https://" />
        </Field>
      </div>
    </div>
  );
}

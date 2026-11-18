import { Link } from "react-router";
import { Field, Select } from "../../components/ui";
import { useCoupons } from "./api";

/** On a sign-up form's thank-you screen: give each new sign-up their own code from a coupon. */
export function FormCouponField({ value, onChange }: { value: string | null | undefined; onChange: (coupon: string | undefined) => void }) {
  const coupons = useCoupons();
  const list = coupons.data?.data ?? [];
  const current = value ?? "";
  const missing = current !== "" && coupons.isSuccess && !list.some((c) => c.name.toLowerCase() === current.toLowerCase());
  return (
    <Field
      label="Unique code from a coupon"
      error={missing ? "There's no coupon with this name anymore." : null}
      hint={
        list.length === 0 && coupons.isSuccess ? (
          <>
            Give each sign-up their own single-use code. <Link to="/content/coupons" className="underline">Create a coupon</Link> first.
          </>
        ) : (
          "Each new sign-up gets their own code instead of the discount code above (after confirming, on double opt-in lists). People already in your audience don't see one here, since anyone can type their address; send it in your welcome email instead."
        )
      }
    >
      <Select value={current} onChange={(e) => onChange(e.target.value || undefined)} aria-label="Coupon">
        <option value="">None (use the discount code above)</option>
        {missing ? <option value={current}>{current}</option> : null}
        {list.map((c) => (
          <option key={c.id} value={c.name}>
            {c.name}
          </option>
        ))}
      </Select>
    </Field>
  );
}

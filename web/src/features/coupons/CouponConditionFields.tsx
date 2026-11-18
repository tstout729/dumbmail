import type { Condition } from "../../api";
import { Input, Select } from "../../components/ui";
import { useCoupons } from "./api";
import type { CouponCondition } from "./types";

const small = "h-8 w-auto text-xs";

export function newCouponCondition(): CouponCondition {
  return { type: "coupon", coupon_id: "", action: "given", op: "has" };
}

export function isCouponConditionComplete(c: CouponCondition): boolean {
  return c.coupon_id !== "" && (c.within_days === undefined || (c.within_days >= 1 && c.within_days <= 3650));
}

/** Segment rule: got a code from a coupon, or used it in an order recorded by the store. */
export function CouponConditionFields({ condition: c, onChange }: { condition: CouponCondition; onChange: (c: Condition) => void }) {
  const coupons = useCoupons();
  return (
    <>
      <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as CouponCondition["op"] })} className={small} aria-label="Has or has not">
        <option value="has">has</option>
        <option value="has_not">has not</option>
      </Select>
      <Select value={c.action} onChange={(e) => onChange({ ...c, action: e.target.value as CouponCondition["action"] })} className={small} aria-label="Given or redeemed">
        <option value="given">been given a code from</option>
        <option value="redeemed">redeemed a code from</option>
      </Select>
      <Select value={c.coupon_id} onChange={(e) => onChange({ ...c, coupon_id: e.target.value })} className={`${small} min-w-40`} aria-label="Coupon">
        {c.coupon_id === "" ? <option value="">Choose a coupon…</option> : null}
        {(coupons.data?.data ?? []).map((x) => (
          <option key={x.id} value={x.id}>
            {x.name}
          </option>
        ))}
      </Select>
      <Select value={c.within_days === undefined ? "all" : "within"} onChange={(e) => onChange({ ...c, within_days: e.target.value === "all" ? undefined : 30 })} className={small} aria-label="Time window">
        <option value="all">ever</option>
        <option value="within">in the last</option>
      </Select>
      {c.within_days !== undefined ? (
        <>
          <Input
            type="number"
            min={1}
            max={3650}
            value={c.within_days}
            onChange={(e) => onChange({ ...c, within_days: Math.max(1, Math.min(3650, Number(e.target.value) || 1)) })}
            className="h-8 w-20 text-xs"
            aria-label="Days"
          />
          <span className="text-xs text-zinc-500">days</span>
        </>
      ) : null}
    </>
  );
}

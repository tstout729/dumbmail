export type CouponAlphabet = "letters_digits" | "letters" | "digits";
export type CouponPattern = { prefix?: string; length: number; alphabet?: CouponAlphabet };

export type CouponStock = { total: number; available: number; assigned: number; redeemed: number; redeemed_unverified: number };

export type CouponWaiting = { type: "campaign" | "flow"; id: string; name: string | null; since: string };

export type Coupon = {
  id: string;
  name: string;
  description: string;
  source: "uploaded" | "generated";
  pattern: CouponPattern | null;
  expires_at: string | null;
  expires_after_days: number | null;
  new_code_each_time: boolean;
  low_stock_threshold: number | null;
  form_codes_per_hour: number;
  codes_total: number;
  out_of_codes_at: string | null;
  created_at: string;
  updated_at: string;
  stock: CouponStock;
  alerts: { out_of_codes: boolean; out_of_codes_since: string | null; low_stock: boolean; waiting: CouponWaiting[] };
  usage: { tag: string };
};

export type CouponInput = {
  name?: string;
  description?: string;
  source?: "uploaded" | "generated";
  pattern?: CouponPattern | null;
  expires_at?: string | null;
  expires_after_days?: number | null;
  new_code_each_time?: boolean;
  low_stock_threshold?: number | null;
  form_codes_per_hour?: number;
};

export type CouponCodeStatus = "available" | "assigned" | "redeemed";

export type CouponCode = {
  id: number;
  code: string;
  status: CouponCodeStatus;
  profile_id: string | null;
  email: string | null;
  given_at: string | null;
  delivered_at: string | null;
  expires_at: string | null;
  redeemed_at: string | null;
  redeemed_verified: boolean | null;
  order_value: number | null;
  given_via: "message" | "form" | null;
};

export type CouponCodeQuery = { q?: string; status?: CouponCodeStatus | ""; limit?: number; offset?: number };
export type CouponCodePage = { data: CouponCode[]; total: number; total_capped: boolean };

export type AddCodesResult = { added: number; duplicates: number; refused: number; coupon: Coupon };
export type GenerateResult = { added: number; coupon: Coupon };

export type CouponReport = {
  given: number;
  redeemed: number;
  redeemed_unverified: number;
  redemption_rate: number;
  revenue: number;
  stock: CouponStock;
  days: { date: string; given: number; redeemed: number; revenue: number }[];
  note: string;
};

/** The email builder's coupon block: the person's code in a dashed box. */
export type CouponBlock = {
  type: "coupon";
  coupon: string;
  title?: string;
  show_expiry?: boolean;
  button_text?: string;
  href?: string;
};

/** Segment rule: got a code from a coupon, or used it in a verified order. */
export type CouponCondition = {
  type: "coupon";
  coupon_id: string;
  action: "given" | "redeemed";
  op: "has" | "has_not";
  within_days?: number;
};

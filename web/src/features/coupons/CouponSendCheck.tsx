import { Link } from "react-router";
import { useCouponCampaignCheck } from "./api";

/** In a campaign's "send now" dialog: whether its coupons have enough codes (from the saved campaign). */
export function CouponSendCheck({ campaignId }: { campaignId: string }) {
  const check = useCouponCampaignCheck(campaignId);
  const notes = [...(check.data?.problems ?? []), ...(check.data?.warnings ?? [])];
  if (!notes.length) return null;
  return (
    <div className="mt-3 rounded-md border border-amber-200 bg-amber-50 px-3 py-2 text-amber-900">
      <ul className="list-disc space-y-1 pl-4">
        {notes.map((n) => (
          <li key={n}>{n}</li>
        ))}
      </ul>
      <Link to="/content/coupons" className="mt-1 inline-block underline">
        Go to coupons
      </Link>
    </div>
  );
}

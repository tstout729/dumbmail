import { useEffect, useRef } from "react";
import { ShieldAlert, Loader2 } from "lucide-react";
import { Notice } from "../../components/ui";
import { useRefreshReview, useSendingReview } from "./api";
import { nextStep } from "./SendingReviewCard";
import { ReviewForm } from "./ReviewForm";

/**
 * Before a campaign goes out: whether the account may send yet (with the questions for a new
 * account), whether this campaign is held for a person, and "Checking your email…" while a send
 * or schedule waits for its check.
 */
export function SendingReviewNotice({ campaignId, checking }: { campaignId: string; checking: boolean }) {
  const review = useSendingReview();
  const refresh = useRefreshReview();
  const was = useRef(checking);
  useEffect(() => {
    // A send or schedule just finished (or was refused): show what the check decided.
    if (was.current && !checking) refresh();
    was.current = checking;
  }, [checking, refresh]);
  const r = review.data;
  if (!r?.enabled) return null;
  const held = r.held?.find((h) => h.subject === "campaign" && h.subject_id === campaignId);
  return (
    <div className="space-y-3">
      {checking ? (
        <Notice tone="blue" icon={<Loader2 className="animate-spin" />} title="Checking your email…">
          Every send is checked against our sending rules first. This usually takes a few seconds.
        </Notice>
      ) : null}
      {!r.can_send ? (
        <Notice tone={r.status === "blocked" ? "red" : "amber"} icon={<ShieldAlert />} title={r.status === "pending" ? "Your account is checked before its first send" : (r.message ?? "")}>
          {nextStep(r)}
          {r.note && r.status !== "pending" ? <p className="mt-1">{r.note}</p> : null}
        </Notice>
      ) : null}
      {r.status === "pending" && r.waiting_for === "answers" ? (
        <div className="rounded-lg border border-zinc-200 p-4">
          <ReviewForm submitLabel="Save answers" />
        </div>
      ) : null}
      {held ? (
        <Notice tone="amber" icon={<ShieldAlert />} title={held.verdict === "block" ? "This campaign can't be sent" : "A person is checking this campaign"}>
          {held.reasons.join(" ")}
          {held.verdict !== "block" ? " We'll email you when it's done; then send it again." : null}
        </Notice>
      ) : null}
    </div>
  );
}

import { ShieldAlert, ShieldCheck, ShieldX, Hourglass } from "lucide-react";
import type { ReactNode } from "react";
import { Badge, Card, Notice, Skeleton, type Tone } from "../../components/ui";
import { useSession } from "../../api";
import { formatDateTime } from "../../lib/format";
import { useSendingReview } from "./api";
import { ReviewForm } from "./ReviewForm";
import type { ReviewStatus, SendingReview } from "./types";

const labels: Record<ReviewStatus, { label: string; tone: Tone }> = {
  pending: { label: "Not checked yet", tone: "amber" },
  approved: { label: "Approved to send", tone: "green" },
  review: { label: "A person is checking", tone: "amber" },
  blocked: { label: "Can't send", tone: "red" },
};

/** What the account still needs to do, in a sentence. */
export function nextStep(r: SendingReview): string | null {
  switch (r.waiting_for) {
    case "answers":
      return "Before your first send, answer three quick questions below. We check every new account so the people who send through DumbMail all keep landing in the inbox.";
    case "sending_domain":
      return "Next step: verify a sending domain in Settings → Domains. We check your account as soon as it's verified.";
    case "check":
      return r.checking ? "We're checking your account now." : "We check your account when you first send. It usually takes a few minutes.";
    case "person":
      return "A person is checking your account. We'll email you, usually within a day.";
    default:
      return null;
  }
}

function icon(s: ReviewStatus): ReactNode {
  return s === "approved" ? <ShieldCheck /> : s === "blocked" ? <ShieldX /> : s === "review" ? <ShieldAlert /> : <Hourglass />;
}

function Reasons({ reasons }: { reasons: string[] }) {
  if (!reasons.length) return null;
  return (
    <ul className="mt-1 list-disc space-y-0.5 pl-4">
      {reasons.map((r) => (
        <li key={r}>{r}</li>
      ))}
    </ul>
  );
}

/** The account's sending review on the settings pages: status, why, and the questions or a review request. */
export function SendingReviewCard() {
  const { account, support_email } = useSession();
  const review = useSendingReview();
  if (review.isPending) {
    return (
      <Card title="Account review">
        <Skeleton className="h-10" />
      </Card>
    );
  }
  if (review.isError) {
    return (
      <Card title="Account review">
        <p className="text-sm text-zinc-500">Couldn't load your account review. Reload the page to try again.</p>
      </Card>
    );
  }
  const r = review.data;
  if (!r.enabled) return null;
  const s = labels[r.status];
  const step = nextStep(r);
  const latest = r.recent?.find((x) => x.kind === "account" && x.reasons.length);
  return (
    <Card title="Account review" actions={<Badge tone={s.tone} dot>{s.label}</Badge>}>
      <div className="space-y-4">
        {r.status === "approved" ? (
          <p className="text-sm text-zinc-600">
            Your account is approved to send. We still check new domains, imported files and what you send, so everyone who sends through DumbMail keeps
            landing in the inbox.
          </p>
        ) : (
          <Notice tone={r.status === "blocked" ? "red" : "amber"} icon={icon(r.status)} title={r.status === "pending" ? "We check new accounts before their first send" : r.message}>
            {step ?? (r.status === "blocked" && support_email ? (
              <>
                If you think this is a mistake, write to{" "}
                <a className="font-medium underline" href={`mailto:${support_email}?subject=${encodeURIComponent(`Account review: ${account.name}`)}`}>
                  {support_email}
                </a>
                . You can still export your data or close the account.
              </>
            ) : null)}
            {r.note && r.status !== "pending" ? <p className="mt-1">{r.note}</p> : null}
            {!r.note && latest && r.status !== "pending" ? <Reasons reasons={latest.reasons} /> : null}
            {r.since && r.status !== "pending" ? <p className="mt-1 text-xs opacity-75">Since {formatDateTime(r.since, account.timezone)}</p> : null}
          </Notice>
        )}
        {r.held?.length ? (
          <div className="space-y-2">
            <div className="text-sm font-medium text-zinc-900">Held for a person's check</div>
            {r.held.map((h) => (
              <div key={h.screening_id ?? h.created_at} className="rounded-md border border-zinc-200 px-3 py-2 text-sm">
                <div className="font-medium text-zinc-800">
                  {h.subject === "flow" ? "Flow" : "Campaign"} {h.name ? `"${h.name}"` : ""}
                </div>
                <div className="text-zinc-600">
                  <Reasons reasons={h.reasons} />
                </div>
              </div>
            ))}
          </div>
        ) : null}
        {r.status === "pending" && r.waiting_for !== "sending_domain" ? (
          <ReviewForm initial={r.answers} submitLabel={r.answers ? "Check my account" : "Request review"} />
        ) : null}
      </div>
    </Card>
  );
}

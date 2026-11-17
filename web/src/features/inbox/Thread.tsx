import { useState, type KeyboardEvent } from "react";
import { Link } from "react-router";
import { ArrowLeft, CheckCircle2, Eye, Paperclip, RotateCcw, Send, User } from "lucide-react";
import { errorMessage, useProfile, useSession } from "../../api";
import { Badge, Button, InlineError, Notice, Select, Skeleton, StatusBadge, Textarea, cx, type Tone } from "../../components/ui";
import { formatDateTime, formatPhone, fullName } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useConversation, usePreviewReply, useSavedReplies, useSendReply, useSetConversationStatus } from "./api";
import { personLabel } from "./InboxPage";
import type { ConversationDetail, ThreadMessage } from "./types";

/** Most texts a thread shows at once; older history is on the API (`before`). */
const MAX_SHOWN = 500;

export function Thread({ id }: { id: string }) {
  // Showing more grows the page instead of paging by date, so the live refresh can't leave gaps.
  const [limit, setLimit] = useState(100);
  const detail = useConversation(id, limit);
  if (detail.isPending) {
    return (
      <div className="flex h-full flex-col gap-3 rounded-lg border border-zinc-200 bg-white p-4 shadow-xs">
        <Skeleton className="h-6 w-48" />
        <Skeleton className="h-16 w-2/3" />
        <Skeleton className="ml-auto h-16 w-1/2" />
      </div>
    );
  }
  if (detail.error) {
    return (
      <div className="rounded-lg border border-zinc-200 bg-white p-4">
        <BackLink />
        <p className="mt-3 text-sm text-red-600">{detail.error.message}</p>
      </div>
    );
  }
  const more = detail.data.has_more && limit < MAX_SHOWN ? () => setLimit((l) => Math.min(MAX_SHOWN, l + 100)) : null;
  return <ThreadView id={id} detail={detail.data} onShowOlder={more} loadingOlder={detail.isFetching && detail.isPlaceholderData} />;
}

function BackLink() {
  return (
    <Link to="/inbox" className="inline-flex items-center gap-1 text-xs text-zinc-500 hover:text-zinc-900 lg:hidden">
      <ArrowLeft className="size-3.5" /> All conversations
    </Link>
  );
}

type ThreadViewProps = { id: string; detail: ConversationDetail; onShowOlder: (() => void) | null; loadingOlder: boolean };

function ThreadView({ id, detail, onShowOlder, loadingOlder }: ThreadViewProps) {
  const { conversation, reply, messages } = detail;
  const setStatus = useSetConversationStatus();
  const p = conversation.profile;
  const closed = conversation.status === "closed";
  // Newest first from the API; the column is reversed so it opens scrolled to the newest text.
  return (
    <div className="grid h-full min-h-0 grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1fr)_260px]">
      <div className="flex min-h-[70dvh] flex-col rounded-lg border border-zinc-200 bg-white shadow-xs lg:min-h-0">
        <header className="flex flex-wrap items-center justify-between gap-2 border-b border-zinc-100 px-4 py-3">
          <div className="min-w-0">
            <BackLink />
            <h2 className="truncate text-sm font-semibold text-zinc-950">{personLabel(p)}</h2>
            <div className="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
              {fullName(p) ? <span className="tabular-nums">{formatPhone(p.phone_number)}</span> : null}
              <StatusBadge status={p.sms_consent} label={consentLabel(p.sms_consent)} />
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Link to={`/audience/profiles/${p.id}`} className="inline-flex items-center gap-1 text-xs font-medium text-zinc-600 hover:text-zinc-900 xl:hidden">
              <User className="size-3.5" /> Profile
            </Link>
            <Button
              size="sm"
              variant={closed ? "secondary" : "primary"}
              icon={closed ? <RotateCcw className="size-3.5" /> : <CheckCircle2 className="size-3.5" />}
              loading={setStatus.isPending}
              onClick={() => setStatus.mutate({ id, status: closed ? "open" : "closed" }, { onSuccess: () => toast.success(closed ? "Reopened" : "Closed") })}
            >
              {closed ? "Reopen" : "Close"}
            </Button>
          </div>
        </header>
        <div className="flex min-h-0 flex-1 flex-col-reverse gap-3 overflow-y-auto px-4 py-4" aria-label="Messages" role="log">
          {messages.map((m) => (
            <Bubble key={m.id} m={m} />
          ))}
          {onShowOlder ? (
            <div className="flex justify-center">
              <Button size="sm" variant="ghost" loading={loadingOlder} onClick={onShowOlder}>
                Show older texts
              </Button>
            </div>
          ) : null}
        </div>
        <ReplyBox key={id} id={id} reply={reply} />
      </div>
      <ProfilePanel profileId={p.id} />
    </div>
  );
}

function consentLabel(c: string): string {
  return { subscribed: "Subscribed to texts", unsubscribed: "Texted STOP", never_subscribed: "Not subscribed", pending: "Confirming sign-up" }[c] ?? c;
}

const kindBadges: Partial<Record<ThreadMessage["kind"], { tone: Tone; label: string }>> = {
  stop: { tone: "red", label: "Opted out" },
  start: { tone: "green", label: "Opted back in" },
  help: { tone: "amber", label: "Asked for help" },
  confirm: { tone: "green", label: "Confirmed sign-up" },
};

function Bubble({ m }: { m: ThreadMessage }) {
  const { account } = useSession();
  const inbound = m.direction === "inbound";
  const badge = m.kind === "keyword" ? { tone: "violet" as Tone, label: `Keyword ${m.keyword ?? ""}` } : kindBadges[m.kind];
  const source = m.campaign ? `Campaign: ${m.campaign.name ?? "deleted"}` : m.flow ? `Flow: ${m.flow.name ?? "deleted"}` : m.kind === "auto_reply" ? "Automatic reply" : m.sent_by ? `Sent by ${m.sent_by}` : m.kind === "reply" ? "Sent over the API" : null;
  const failed = m.status === "failed" || m.status === "bounced";
  return (
    <div className={cx("flex max-w-[85%] flex-col gap-1 sm:max-w-[70%]", inbound ? "items-start self-start" : "items-end self-end")}>
      <div
        className={cx(
          "whitespace-pre-wrap break-words rounded-2xl px-3.5 py-2 text-sm",
          inbound ? "rounded-bl-sm bg-zinc-100 text-zinc-900" : m.kind === "reply" ? "rounded-br-sm bg-accent-700 text-white" : "rounded-br-sm bg-sky-50 text-zinc-900 ring-1 ring-sky-100",
        )}
      >
        {m.body ? m.body : <span className="italic opacity-70">{inbound ? "(no text)" : "Text not kept for messages sent before the inbox was turned on"}</span>}
        {m.media.length > 0 ? (
          <div className="mt-2 flex flex-wrap gap-2">
            {m.media.map((media, i) =>
              media.stored ? (
                <a key={i} href={media.url} target="_blank" rel="noreferrer">
                  <img src={media.url} alt="Picture they texted" className="max-h-48 rounded-lg" />
                </a>
              ) : (
                <a key={i} href={media.url} target="_blank" rel="noopener noreferrer nofollow" className={cx("inline-flex items-center gap-1 text-xs underline", inbound || m.kind !== "reply" ? "text-zinc-700" : "text-white")}>
                  <Paperclip className="size-3" /> {media.content_type ?? "Attachment"}
                </a>
              ),
            )}
          </div>
        ) : null}
      </div>
      <div className="flex flex-wrap items-center gap-1.5 text-[11px] text-zinc-400">
        {badge ? <Badge tone={badge.tone}>{badge.label}</Badge> : null}
        {source ? <span>{source}</span> : null}
        <span>{formatDateTime(m.at, account.timezone)}</span>
        {failed ? <span className="text-red-600">Not delivered{m.error ? `: ${m.error}` : ""}</span> : null}
      </div>
    </div>
  );
}

function ReplyBox({ id, reply }: { id: string; reply: ConversationDetail["reply"] }) {
  const { account } = useSession();
  const [body, setBody] = useState("");
  const saved = useSavedReplies();
  const send = useSendReply();
  const preview = usePreviewReply();
  const [shown, setShown] = useState<string | null>(null);

  if (!reply.allowed) {
    return (
      <div className="border-t border-zinc-100 p-3">
        <Notice tone="gray" title="You can't reply to this person right now">
          {reply.reason}
        </Notice>
      </div>
    );
  }
  const submit = () => {
    if (!body.trim()) return;
    send.mutate(
      { id, body },
      {
        onSuccess: () => {
          setBody("");
          setShown(null);
        },
      },
    );
  };
  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submit();
    }
  };
  return (
    <div className="space-y-2 border-t border-zinc-100 p-3">
      {reply.window_ends_at ? (
        <p className="text-xs text-zinc-500">
          They aren't subscribed to texts, so you can reply until {formatDateTime(reply.window_ends_at, account.timezone)} (24 hours after their last text).
        </p>
      ) : null}
      <Textarea
        value={body}
        onChange={(e) => {
          setBody(e.target.value);
          setShown(null);
        }}
        onKeyDown={onKeyDown}
        rows={3}
        maxLength={1600}
        placeholder="Write a reply… {{ first_name|default('there') }} fills in their name."
        aria-label="Reply"
      />
      {shown !== null ? (
        <div className="rounded-md bg-zinc-50 px-3 py-2 text-xs text-zinc-700">
          <div className="mb-0.5 font-medium text-zinc-500">How it goes out</div>
          <div className="whitespace-pre-wrap">{shown}</div>
        </div>
      ) : null}
      <InlineError message={send.error ? errorMessage(send.error) : preview.error ? errorMessage(preview.error) : null} />
      <div className="flex flex-wrap items-center justify-between gap-2">
        {saved.data && saved.data.length > 0 ? (
          <Select
            value=""
            onChange={(e) => {
              const r = saved.data.find((s) => s.id === e.target.value);
              if (r) setBody((b) => (b.trim() ? `${b.trimEnd()} ${r.body}` : r.body));
            }}
            className="w-48"
            aria-label="Insert a saved reply"
          >
            <option value="">Insert saved reply…</option>
            {saved.data.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}
              </option>
            ))}
          </Select>
        ) : (
          <Link to="/settings/inbox" className="text-xs text-zinc-500 hover:text-zinc-900">
            Add saved replies
          </Link>
        )}
        <div className="flex items-center gap-2">
          <Button
            size="sm"
            variant="ghost"
            icon={<Eye className="size-3.5" />}
            disabled={!body.trim()}
            loading={preview.isPending}
            onClick={() => preview.mutate({ id, body }, { onSuccess: (r) => setShown(`${r.body}\n\n${r.segments === 1 ? "1 segment" : `${r.segments} segments`}`) })}
          >
            Preview
          </Button>
          <Button variant="primary" size="sm" icon={<Send className="size-3.5" />} disabled={!body.trim()} loading={send.isPending} onClick={submit}>
            Send
          </Button>
        </div>
      </div>
    </div>
  );
}

function ProfilePanel({ profileId }: { profileId: string }) {
  const detail = useProfile(profileId);
  return (
    <aside className="hidden min-h-0 overflow-y-auto rounded-lg border border-zinc-200 bg-white p-4 shadow-xs xl:block" aria-label="Profile">
      {detail.isPending ? (
        <Skeleton className="h-40" />
      ) : detail.error ? (
        <p className="text-sm text-red-600">{detail.error.message}</p>
      ) : (
        <div className="space-y-4 text-sm">
          <div>
            <div className="font-semibold text-zinc-950">{fullName(detail.data.profile) || "No name"}</div>
            <div className="text-xs text-zinc-500 tabular-nums">{formatPhone(detail.data.profile.phone_number)}</div>
            {detail.data.profile.email ? <div className="truncate text-xs text-zinc-500">{detail.data.profile.email}</div> : null}
          </div>
          <dl className="space-y-2 text-xs">
            <div className="flex items-center justify-between gap-2">
              <dt className="text-zinc-500">Texts</dt>
              <dd>
                <StatusBadge status={detail.data.profile.sms_consent} label={consentLabel(detail.data.profile.sms_consent)} />
              </dd>
            </div>
            <div className="flex items-center justify-between gap-2">
              <dt className="text-zinc-500">Email</dt>
              <dd>
                <StatusBadge status={detail.data.profile.email_consent} />
              </dd>
            </div>
          </dl>
          {detail.data.lists.length > 0 ? (
            <div>
              <div className="mb-1 text-xs font-medium text-zinc-500">Lists</div>
              <div className="flex flex-wrap gap-1">
                {detail.data.lists.map((l) => (
                  <Badge key={l.id}>{l.name}</Badge>
                ))}
              </div>
            </div>
          ) : null}
          {detail.data.segments.length > 0 ? (
            <div>
              <div className="mb-1 text-xs font-medium text-zinc-500">Segments</div>
              <div className="flex flex-wrap gap-1">
                {detail.data.segments.slice(0, 12).map((s) => (
                  <Badge key={s.id} tone="blue">
                    {s.name}
                  </Badge>
                ))}
              </div>
            </div>
          ) : null}
          <Link to={`/audience/profiles/${profileId}`} className="inline-flex items-center gap-1 text-xs font-medium text-accent-700 hover:text-accent-800">
            <User className="size-3.5" /> Open full profile
          </Link>
        </div>
      )}
    </aside>
  );
}

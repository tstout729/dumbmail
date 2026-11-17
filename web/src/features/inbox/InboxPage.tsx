import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { Inbox, KeyRound, MessageSquare, Search, Settings2 } from "lucide-react";
import { Button, ButtonLink, Checkbox, EmptyState, Input, Notice, Page, PageHeader, Segmented, Skeleton, cx } from "../../components/ui";
import { useDebouncedValue } from "../../lib/hooks";
import { formatPhone, formatRelative, fullName } from "../../lib/format";
import { useCan } from "../team/api";
import { useConversations, useInboxSettings, useMarkRead, type ConversationFilter } from "./api";
import { Thread } from "./Thread";
import type { Conversation, ConversationStatus } from "./types";

export function personLabel(p: Conversation["profile"]): string {
  return fullName(p) || formatPhone(p.phone_number) || p.email || "Unknown";
}

export function InboxPage() {
  const { id = "" } = useParams();
  const [status, setStatus] = useState<ConversationStatus | "all">("open");
  const [unread, setUnread] = useState(false);
  const [search, setSearch] = useState("");
  const q = useDebouncedValue(search.trim(), 300);
  const filter: ConversationFilter = { status, unread, q };
  const canRead = useCan("profiles.read");

  return (
    <Page wide>
      <PageHeader
        title="Inbox"
        description="Texts people send to your number, and your replies."
        actions={
          <>
            <ButtonLink to="/inbox/keywords" icon={<KeyRound className="size-3.5" />}>
              Keywords
            </ButtonLink>
            <ButtonLink to="/settings/inbox" icon={<Settings2 className="size-3.5" />}>
              Settings
            </ButtonLink>
          </>
        }
      />
      {canRead ? <AutoPausedNotice /> : null}
      {!canRead ? (
        <Notice tone="gray" title="Your role can't see texts">
          Texts are people's own messages, so only roles that can see profiles can open the inbox. Ask an owner or admin if you need it.
        </Notice>
      ) : (
        <div className="grid grid-cols-1 gap-4 lg:h-[calc(100dvh-9.5rem)] lg:min-h-[520px] lg:grid-cols-[340px_minmax(0,1fr)]">
          <section className={cx("min-h-0 flex-col rounded-lg border border-zinc-200 bg-white shadow-xs", id ? "hidden lg:flex" : "flex")}>
            <div className="space-y-2.5 border-b border-zinc-100 p-3">
              <div className="relative">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-zinc-400" />
                <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search names, numbers, texts" className="pl-8" aria-label="Search conversations" />
              </div>
              <div className="flex flex-wrap items-center justify-between gap-2">
                <Segmented
                  size="sm"
                  value={status}
                  onChange={setStatus}
                  options={[
                    { value: "open", label: "Open" },
                    { value: "closed", label: "Closed" },
                    { value: "all", label: "All" },
                  ]}
                />
                <Checkbox label="Unread only" checked={unread} onChange={(e) => setUnread(e.target.checked)} />
              </div>
            </div>
            <ConversationList filter={filter} selected={id} />
          </section>
          <section className={cx("min-h-0", id ? "block" : "hidden lg:block")}>
            {id ? (
              <Thread key={id} id={id} />
            ) : (
              <div className="flex h-full items-center justify-center rounded-lg border border-dashed border-zinc-200 bg-white">
                <EmptyState icon={<MessageSquare className="size-5" />} title="Pick a conversation" description="Choose someone on the left to read their texts and reply." />
              </div>
            )}
          </section>
        </div>
      )}
    </Page>
  );
}

/** Automatic texts stop for the rest of the hour when an hourly budget runs out. */
function AutoPausedNotice() {
  const settings = useInboxSettings();
  const s = settings.data;
  if (!s?.auto_paused) return null;
  const which = s.auto_budget_hit_scope === "account" ? "Your account's" : `The ${formatPhone(s.auto_budget_hit_scope ?? "")}`;
  return (
    <div className="mb-4">
      <Notice tone="amber" title="Automatic texts are paused until the next hour">
        {which} hourly limit on automatic texts (keyword answers, sign-up texts, automatic replies, HELP answers) ran out. You can still reply yourself. Change the
        limits in{" "}
        <Link to="/settings/inbox" className="underline">
          Inbox settings
        </Link>
        .
      </Notice>
    </div>
  );
}

function ConversationList({ filter, selected }: { filter: ConversationFilter; selected: string }) {
  const list = useConversations(filter);
  const markRead = useMarkRead();
  const navigate = useNavigate();
  if (list.isPending) {
    return (
      <div className="space-y-3 p-3">
        {Array.from({ length: 6 }, (_, i) => (
          <Skeleton key={i} className="h-12" />
        ))}
      </div>
    );
  }
  if (list.error) return <p className="p-4 text-sm text-red-600">{list.error.message}</p>;
  const rows = list.data.pages.flatMap((p) => p.data);
  if (rows.length === 0) {
    const filtered = filter.q !== "" || filter.unread || filter.status !== "open";
    return (
      <EmptyState
        icon={<Inbox className="size-5" />}
        title={filtered ? "No conversations match" : "No texts yet"}
        description={
          filtered ? (
            "Try another search or filter."
          ) : (
            <>
              When someone texts your number, it shows up here. Set up a number in <Link to="/settings/sms" className="text-accent-700 underline">Settings → SMS</Link>, and keywords like
              JOIN in <Link to="/inbox/keywords" className="text-accent-700 underline">Keywords</Link>.
            </>
          )
        }
      />
    );
  }
  return (
    <ul className="min-h-0 flex-1 divide-y divide-zinc-100 overflow-y-auto" aria-label="Conversations">
      {rows.map((c) => (
        <li key={c.id}>
          <button
            type="button"
            onClick={() => {
              if (c.unread_count > 0) markRead.mutate(c.id);
              navigate(`/inbox/${c.id}`);
            }}
            className={cx("flex w-full items-start gap-3 px-3 py-2.5 text-left transition-colors hover:bg-zinc-50", c.id === selected && "bg-accent-50/60 hover:bg-accent-50")}
            aria-current={c.id === selected ? "true" : undefined}
          >
            <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-zinc-100 text-xs font-semibold text-zinc-600">
              {(c.profile.first_name ?? "#").slice(0, 1).toUpperCase()}
            </span>
            <span className="min-w-0 flex-1">
              <span className="flex items-baseline justify-between gap-2">
                <span className={cx("truncate text-sm", c.unread_count > 0 ? "font-semibold text-zinc-950" : "font-medium text-zinc-800")}>{personLabel(c.profile)}</span>
                <span className="shrink-0 text-[11px] text-zinc-400">{formatRelative(c.last_message_at)}</span>
              </span>
              <span className="mt-0.5 flex items-center gap-2">
                <span className={cx("min-w-0 flex-1 truncate text-xs", c.unread_count > 0 ? "text-zinc-800" : "text-zinc-500")}>
                  {c.last_direction === "outbound" ? "You: " : ""}
                  {c.last_preview || "No text"}
                </span>
                {c.unread_count > 0 ? (
                  <span className="shrink-0 rounded-full bg-accent-700 px-1.5 text-[11px] font-semibold text-white" aria-label={`${c.unread_count} unread`}>
                    {c.unread_count}
                  </span>
                ) : null}
              </span>
            </span>
          </button>
        </li>
      ))}
      {list.hasNextPage ? (
        <li className="flex justify-center p-2">
          <Button size="sm" variant="ghost" loading={list.isFetchingNextPage} onClick={() => void list.fetchNextPage()}>
            Show more
          </Button>
        </li>
      ) : null}
    </ul>
  );
}

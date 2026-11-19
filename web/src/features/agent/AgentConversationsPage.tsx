import { useState } from "react";
import { Link, useParams } from "react-router";
import { ArrowLeft, Bot, MessageSquare, Settings2 } from "lucide-react";
import { Button, ButtonLink, EmptyState, InlineError, Notice, Page, PageHeader, Segmented, Skeleton, cx } from "../../components/ui";
import { formatPhone, formatRelative } from "../../lib/format";
import { useCan } from "../team/api";
import { useAgentConversation, useAgentConversations } from "./api";
import { PLACE_LABEL, REASON_LABEL, StatusLabel } from "./parts";
import type { AgentConversation } from "./types";

type Filter = "all" | "handed_off";

function who(c: AgentConversation): string {
  if (!c.person) return c.place === "test" ? "You (test)" : "Anonymous shopper";
  return c.person.name || c.person.email || formatPhone(c.person.phone_number) || "Shopper";
}

/** Inbox → Shopping agent: what the agent said to shoppers, with hand-offs marked. */
export function AgentConversationsPage() {
  const { id = "" } = useParams();
  const [filter, setFilter] = useState<Filter>("all");
  const canRead = useCan("profiles.read");
  const list = useAgentConversations(filter === "handed_off" ? true : null, canRead);
  const rows = list.data?.pages.flatMap((p) => p.data) ?? [];
  const handed = list.data?.pages[0]?.handed_off_count ?? 0;
  return (
    <Page wide>
      <PageHeader
        title="Shopping agent"
        description="Conversations the AI assistant had with shoppers. Hand-offs are the ones that need a person."
        breadcrumbs={[{ label: "Inbox", to: "/inbox" }]}
        actions={
          <ButtonLink to="/settings/shopping-agent" icon={<Settings2 className="size-3.5" />}>
            Settings
          </ButtonLink>
        }
      />
      {!canRead ? (
        <Notice tone="gray" title="Your role can't see these conversations">
          They're shoppers' own words, so only roles that can see profiles can open them. Ask an owner or admin if you need it.
        </Notice>
      ) : (
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-[380px_minmax(0,1fr)]">
          <section className={cx("min-w-0 flex-col rounded-lg border border-zinc-200 bg-white shadow-xs", id ? "hidden lg:flex" : "flex")}>
            <div className="border-b border-zinc-100 p-3">
              <Segmented<Filter>
                size="sm"
                value={filter}
                onChange={setFilter}
                options={[
                  { value: "all", label: "All" },
                  { value: "handed_off", label: handed ? `Handed off (${handed})` : "Handed off" },
                ]}
              />
            </div>
            {list.isPending ? (
              <div className="space-y-2 p-3">
                <Skeleton className="h-14 w-full" />
                <Skeleton className="h-14 w-full" />
                <Skeleton className="h-14 w-full" />
              </div>
            ) : list.error ? (
              <div className="p-3">
                <InlineError message={list.error.message} />
              </div>
            ) : rows.length === 0 ? (
              <EmptyState
                icon={<Bot className="size-5" />}
                title={filter === "handed_off" ? "No hand-offs" : "No conversations yet"}
                description={filter === "handed_off" ? "Nothing is waiting for a person." : "Turn the agent on in Settings → Shopping agent, or try it there."}
              />
            ) : (
              <ul className="divide-y divide-zinc-100">
                {rows.map((c) => (
                  <li key={c.id}>
                    <Link to={`/inbox/agent/${c.id}`} className={cx("block px-3 py-3 hover:bg-zinc-50", c.id === id && "bg-zinc-50")}>
                      <div className="flex items-center justify-between gap-2">
                        <span className="truncate text-sm font-medium text-zinc-900">{who(c)}</span>
                        <span className="shrink-0 text-xs text-zinc-500">{formatRelative(c.last_message_at)}</span>
                      </div>
                      <div className="mt-0.5 flex items-center justify-between gap-2">
                        <span className="truncate text-sm text-zinc-500">{c.handoff_summary || c.last_message || "—"}</span>
                        <StatusLabel c={c} />
                      </div>
                      <div className="mt-0.5 text-xs text-zinc-400">{PLACE_LABEL[c.place]}</div>
                    </Link>
                  </li>
                ))}
              </ul>
            )}
            {list.hasNextPage ? (
              <div className="border-t border-zinc-100 p-3">
                <Button size="sm" onClick={() => void list.fetchNextPage()} loading={list.isFetchingNextPage}>
                  Show more
                </Button>
              </div>
            ) : null}
          </section>
          <section className={cx("min-w-0 rounded-lg border border-zinc-200 bg-white shadow-xs", id ? "block" : "hidden lg:block")}>
            {id ? (
              <Transcript id={id} />
            ) : (
              <EmptyState icon={<MessageSquare className="size-5" />} title="Pick a conversation" description="Its messages and any hand-off show here." />
            )}
          </section>
        </div>
      )}
    </Page>
  );
}

function Transcript({ id }: { id: string }) {
  const c = useAgentConversation(id);
  if (c.isPending) return <Skeleton className="m-4 h-64" />;
  if (c.error) return <div className="p-4"><InlineError message={c.error.message} /></div>;
  const d = c.data;
  return (
    <div className="flex flex-col">
      <div className="flex flex-wrap items-center gap-3 border-b border-zinc-100 p-3">
        <Link to="/inbox/agent" className="text-zinc-500 hover:text-zinc-900 lg:hidden" aria-label="Back to conversations">
          <ArrowLeft className="size-4" />
        </Link>
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-semibold text-zinc-900">{who(d)}</div>
          <div className="text-xs text-zinc-500">
            {PLACE_LABEL[d.place]} · started {formatRelative(d.created_at)} · {d.turns} {d.turns === 1 ? "message" : "messages"}
          </div>
        </div>
        {d.person ? (
          <ButtonLink to={`/audience/profiles/${d.person.id}`} size="sm">
            Profile
          </ButtonLink>
        ) : null}
        {d.sms_conversation_id ? (
          <ButtonLink to={`/inbox/${d.sms_conversation_id}`} size="sm" variant="primary">
            Reply by text
          </ButtonLink>
        ) : null}
      </div>
      {d.handed_off ? (
        <div className="border-b border-zinc-100 p-3">
          <Notice tone="amber" title={`Handed off: ${REASON_LABEL[d.handoff_reason ?? "other"] ?? "Other"}`}>
            {d.handoff_summary || "The agent passed this conversation to your team."}
            {d.place === "site" ? " This shopper chatted anonymously on your website, so there's no one to reply to here." : null}
          </Notice>
        </div>
      ) : null}
      <div className="flex flex-col gap-2 p-4" role="log">
        {d.messages
          .filter((m) => m.role !== "note")
          .map((m, i) => (
            <div key={i} className={cx("max-w-[85%] whitespace-pre-wrap rounded-lg px-3 py-2 text-sm", m.role === "shopper" ? "self-start bg-zinc-100 text-zinc-900" : "self-end bg-accent-600 text-white")}>
              {m.body}
              <div className={cx("mt-1 text-[11px]", m.role === "shopper" ? "text-zinc-500" : "text-accent-100")}>
                {m.role === "shopper" ? "Shopper" : "Agent"} · {formatRelative(m.at)}
              </div>
            </div>
          ))}
      </div>
    </div>
  );
}

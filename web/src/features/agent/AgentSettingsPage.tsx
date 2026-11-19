import { useState, type ReactNode } from "react";
import { Link } from "react-router";
import { Plus, RotateCcw, Send, Trash2 } from "lucide-react";
import { Badge, Button, ButtonLink, Card, Field, IconButton, InlineError, Input, Notice, Skeleton, Textarea, Toggle, cx } from "../../components/ui";
import { formatCurrency, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { useAgentConversations, useAgentSettings, useAgentTest, useUpdateAgentSettings } from "./api";
import { PLACE_LABEL, StatusLabel } from "./parts";
import type { AgentAnswer, AgentPage, AgentSettings, AgentSettingsPatch } from "./types";

const MAX_ANSWERS = 100;
const MAX_PAGES = 20;

/** Settings → Shopping agent: where the AI assistant answers shoppers, and what it knows. */
export function AgentSettingsPage() {
  const settings = useAgentSettings();
  if (settings.isPending) return <Skeleton className="h-96 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  return (
    <div className="space-y-4">
      <AgentForm key={JSON.stringify(settings.data)} s={settings.data} />
      <TestChat s={settings.data} />
      <RecentCard />
    </div>
  );
}

type Draft = { hub: boolean; site: boolean; sms: boolean; name: string; tone: string; answers: AgentAnswer[]; monthly_cap: number; pages: string[] };

function draftOf(s: AgentSettings): Draft {
  return { hub: s.hub, site: s.site, sms: s.sms, name: s.name, tone: s.tone, answers: s.answers.map((a) => ({ ...a })), monthly_cap: s.monthly_cap, pages: s.pages.map((p) => p.url) };
}

function PlaceToggle({ checked, onChange, title, children }: { checked: boolean; onChange: (v: boolean) => void; title: string; children: ReactNode }) {
  return (
    <div className="flex items-start gap-3">
      <Toggle checked={checked} onChange={onChange} label={title} />
      <div className="min-w-0 text-sm">
        <div className="font-medium text-zinc-900">{title}</div>
        <div className="text-zinc-500">{children}</div>
      </div>
    </div>
  );
}

function AgentForm({ s }: { s: AgentSettings }) {
  const canEdit = useCan("settings.write");
  const update = useUpdateAgentSettings();
  const [d, setD] = useState<Draft>(() => draftOf(s));
  const [newPage, setNewPage] = useState("");
  const set = (patch: Partial<Draft>) => setD((x) => ({ ...x, ...patch }));
  const dirty = JSON.stringify(d) !== JSON.stringify(draftOf(s));
  const saved = new Map<string, AgentPage>(s.pages.map((p) => [p.url, p]));
  const addPage = () => {
    const u = newPage.trim();
    if (!u || d.pages.includes(u)) return;
    set({ pages: [...d.pages, u] });
    setNewPage("");
  };
  const save = () => {
    const patch: AgentSettingsPatch = { ...d, answers: d.answers.filter((a) => a.question.trim() || a.answer.trim()) };
    update.mutate(patch, { onSuccess: () => toast.success("Shopping agent saved") });
  };
  const price = s.price_per_conversation_usd;
  return (
    <fieldset disabled={!canEdit} className="min-w-0 space-y-4">
      <Card title="Shopping agent">
        <div className="space-y-3 text-sm text-zinc-600">
          <p>
            An AI assistant that answers shoppers' questions from your own answers, your website pages, your product catalog and, for a shopper it can verify, their own orders. It
            hands the conversation to your team for refunds, complaints and anything it isn't sure about. It can't change orders, give discounts or send marketing.
          </p>
          {!s.available ? (
            <Notice tone="gray" title="Not available on this server">
              The shopping agent needs an Anthropic API key on the server (ANTHROPIC_API_KEY). Until one is set, nothing answers.
            </Notice>
          ) : null}
          <p>
            <span className="font-medium text-zinc-900">{s.conversations_this_month.toLocaleString()}</span> of {s.monthly_cap.toLocaleString()} conversations this month
            {price > 0 ? ` · ${formatCurrency(price, true)} per conversation` : ""}.
          </p>
        </div>
      </Card>

      <Card title="Where it answers">
        <div className="space-y-4">
          <PlaceToggle checked={d.hub} onChange={(v) => set({ hub: v })} title="Customer hub">
            A chat for shoppers signed in to your customer hub. It can look up their own orders and tracking.
            {!s.hub_enabled ? (
              <>
                {" "}
                The hub is off: <Link to="/settings/customer-hub" className="text-accent-700 hover:underline">turn it on</Link> first.
              </>
            ) : null}
          </PlaceToggle>
          <PlaceToggle checked={d.site} onChange={(v) => set({ site: v })} title="Chat bubble on your website">
            A chat button on every page with the DumbMail snippet, for anyone. It never knows who they are or shows anything personal; for order questions it points them to the
            customer hub.
            {s.site_origins.length ? ` It opens over your pages on ${s.site_origins.join(", ")}.` : " Add your website in Settings → Account so it can open over your pages."}
          </PlaceToggle>
          <PlaceToggle checked={d.sms} onChange={(v) => set({ sms: v })} title="First answers to texts">
            Answers ordinary texts to your number before a person does, instead of your default reply. Someone texting from the number on their profile can get their order status.
            Each answer is an automatic text, billed like any reply.
          </PlaceToggle>
        </div>
      </Card>

      <Card title="Name and tone">
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Name" hint="What it calls itself.">
            <Input value={d.name} maxLength={40} placeholder={s.display_name} onChange={(e) => set({ name: e.target.value })} />
          </Field>
          <Field label="Tone" hint="How it should sound. This can't change its rules.">
            <Textarea value={d.tone} maxLength={300} rows={2} placeholder="e.g. Warm and brief. Use the shopper's first name." onChange={(e) => set({ tone: e.target.value })} />
          </Field>
        </div>
      </Card>

      <Card title="Questions and answers">
        <div className="space-y-3">
          <p className="text-sm text-zinc-600">Your own words for what shoppers ask most: shipping, returns, sizing, care. The agent quotes these for policies.</p>
          {d.answers.map((a, i) => (
            <div key={i} className="grid gap-2 rounded-md border border-zinc-200 p-3 sm:grid-cols-[1fr_auto]">
              <div className="space-y-2">
                <Input value={a.question} maxLength={300} placeholder="e.g. Do you ship to Canada?" aria-label={`Question ${i + 1}`} onChange={(e) => set({ answers: d.answers.map((x, j) => (j === i ? { ...x, question: e.target.value } : x)) })} />
                <Textarea value={a.answer} maxLength={2000} rows={2} placeholder="The answer" aria-label={`Answer ${i + 1}`} onChange={(e) => set({ answers: d.answers.map((x, j) => (j === i ? { ...x, answer: e.target.value } : x)) })} />
              </div>
              <IconButton label={`Remove question ${i + 1}`} onClick={() => set({ answers: d.answers.filter((_, j) => j !== i) })}>
                <Trash2 className="size-4" />
              </IconButton>
            </div>
          ))}
          {d.answers.length < MAX_ANSWERS ? (
            <Button size="sm" icon={<Plus className="size-3.5" />} onClick={() => set({ answers: [...d.answers, { question: "", answer: "" }] })}>
              Add a question
            </Button>
          ) : null}
        </div>
      </Card>

      <Card title="Website pages">
        <div className="space-y-3">
          <p className="text-sm text-zinc-600">
            Pages from your own website to answer from, like your shipping, returns or size guide pages. They're read as plain text now and every week.
            {s.website ? ` Only pages on ${s.website} can be added.` : " Add your website in Settings → Account first."}
          </p>
          {d.pages.length ? (
            <ul className="divide-y divide-zinc-100 rounded-md border border-zinc-200">
              {d.pages.map((u) => {
                const p = saved.get(u);
                return (
                  <li key={u} className="flex items-start gap-3 px-3 py-2.5">
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm font-medium text-zinc-900">{p?.title || u}</div>
                      {p?.title ? <div className="truncate text-xs text-zinc-500">{u}</div> : null}
                      {p?.error ? <div className="mt-0.5 text-xs text-red-700">{p.error}</div> : null}
                    </div>
                    {!p ? (
                      <Badge tone="gray">Not saved</Badge>
                    ) : p.status === "ready" ? (
                      <Badge tone="green">{p.fetched_at ? `Read ${formatRelative(p.fetched_at)}` : "Read"}</Badge>
                    ) : p.status === "failed" ? (
                      <Badge tone="red">Couldn't read</Badge>
                    ) : (
                      <Badge tone="amber">Reading soon</Badge>
                    )}
                    <IconButton label={`Remove ${u}`} size="sm" onClick={() => set({ pages: d.pages.filter((x) => x !== u) })}>
                      <Trash2 className="size-4" />
                    </IconButton>
                  </li>
                );
              })}
            </ul>
          ) : null}
          {d.pages.length < MAX_PAGES ? (
            <div className="flex gap-2">
              <Input
                value={newPage}
                placeholder="https://yourbrand.com/shipping"
                aria-label="Page address"
                onChange={(e) => setNewPage(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    addPage();
                  }
                }}
              />
              <Button icon={<Plus className="size-3.5" />} onClick={addPage} disabled={!newPage.trim()}>
                Add
              </Button>
            </div>
          ) : null}
        </div>
      </Card>

      <Card title="Monthly limit">
        <Field label="Most conversations a month" hint="Past it, shoppers are told chat isn't available and given your contact details." className="max-w-xs">
          <Input type="number" min={1} max={100000} value={d.monthly_cap} onChange={(e) => set({ monthly_cap: Math.max(1, Math.round(Number(e.target.value) || 1)) })} />
        </Field>
      </Card>

      {canEdit ? (
        <div className="flex items-center justify-end gap-3">
          <InlineError message={update.error?.message} />
          <Button variant="primary" loading={update.isPending} disabled={!dirty} onClick={save}>
            Save
          </Button>
        </div>
      ) : (
        <p className="text-sm text-zinc-500">You can look at these settings. Changing them needs an Admin or Owner.</p>
      )}
    </fieldset>
  );
}

type Line = { who: "me" | "agent"; text: string; handedOff?: boolean };

/** Ask the agent questions as a made-up shopper with a made-up order. */
function TestChat({ s }: { s: AgentSettings }) {
  const canEdit = useCan("settings.write");
  const test = useAgentTest();
  const [lines, setLines] = useState<Line[]>([]);
  const [conversation, setConversation] = useState<string | null>(null);
  const [text, setText] = useState("");
  const send = () => {
    const message = text.trim();
    if (!message) return;
    setLines((l) => [...l, { who: "me", text: message }]);
    setText("");
    test.mutate(
      { message, conversation },
      {
        onSuccess: (r) => {
          setConversation(r.conversation);
          setLines((l) => [...l, { who: "agent", text: r.reply ?? "", handedOff: r.handed_off }]);
        },
      },
    );
  };
  const reset = () => {
    setLines([]);
    setConversation(null);
    test.reset();
  };
  const off = !s.available || !canEdit;
  return (
    <Card
      title="Try it"
      actions={
        lines.length ? (
          <Button size="sm" variant="ghost" icon={<RotateCcw className="size-3.5" />} onClick={reset}>
            Start over
          </Button>
        ) : null
      }
    >
      <div className="space-y-3">
        <p className="text-sm text-zinc-600">Chat as a made-up shopper with one made-up order, using your saved settings. Nothing is sent to anyone; it counts toward the monthly limit.</p>
        {lines.length ? (
          <div className="flex max-h-96 flex-col gap-2 overflow-y-auto rounded-md border border-zinc-200 bg-zinc-50 p-3" role="log" aria-live="polite">
            {lines.map((l, i) => (
              <div key={i} className={cx("max-w-[85%] whitespace-pre-wrap rounded-lg px-3 py-2 text-sm", l.who === "me" ? "self-end bg-accent-600 text-white" : "self-start border border-zinc-200 bg-white text-zinc-900")}>
                {l.text}
                {l.handedOff ? <div className="mt-1 text-xs text-amber-700">Handed off to your team</div> : null}
              </div>
            ))}
            {test.isPending ? <div className="self-start rounded-lg border border-zinc-200 bg-white px-3 py-2 text-sm text-zinc-500">…</div> : null}
          </div>
        ) : null}
        <div className="flex gap-2">
          <Input
            value={text}
            maxLength={1000}
            disabled={off || test.isPending}
            placeholder={s.available ? "e.g. Where is my order?" : "Not available on this server"}
            aria-label="Test question"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                send();
              }
            }}
          />
          <Button variant="primary" icon={<Send className="size-3.5" />} onClick={send} disabled={off || !text.trim()} loading={test.isPending}>
            Send
          </Button>
        </div>
        <InlineError message={test.error?.message} />
      </div>
    </Card>
  );
}

function RecentCard() {
  const canRead = useCan("profiles.read");
  const list = useAgentConversations(null, canRead);
  if (!canRead) return null;
  const rows = list.data?.pages.flatMap((p) => p.data).slice(0, 5) ?? [];
  const handed = list.data?.pages[0]?.handed_off_count ?? 0;
  return (
    <Card
      title="Recent conversations"
      actions={
        <ButtonLink to="/inbox/agent" size="sm">
          See all{handed ? ` · ${handed} handed off` : ""}
        </ButtonLink>
      }
    >
      {list.isPending ? (
        <Skeleton className="h-24 w-full" />
      ) : list.error ? (
        <InlineError message={list.error.message} />
      ) : rows.length === 0 ? (
        <p className="text-sm text-zinc-500">No conversations yet.</p>
      ) : (
        <ul className="divide-y divide-zinc-100">
          {rows.map((c) => (
            <li key={c.id}>
              <Link to={`/inbox/agent/${c.id}`} className="flex items-center gap-3 py-2.5 hover:bg-zinc-50">
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm text-zinc-900">{c.last_message ?? "—"}</div>
                  <div className="text-xs text-zinc-500">
                    {PLACE_LABEL[c.place]} · {formatRelative(c.last_message_at)}
                  </div>
                </div>
                <StatusLabel c={c} />
              </Link>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

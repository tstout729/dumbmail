import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { ArrowLeft, CheckCircle2, MessageCircle, MessageSquare, RotateCcw, Send, Settings2, User } from "lucide-react";
import { Badge, Button, ButtonLink, EmptyState, Field, InlineError, Input, Notice, Page, PageHeader, Segmented, Select, Skeleton, Textarea, cx } from "../../components/ui";
import { formatDateTime, formatPhone, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useSession } from "../../api";
import { useCan } from "../team/api";
import { useMarkWhatsappRead, useSetWhatsappStatus, useWhatsappConversation, useWhatsappConversations, useWhatsappReply } from "./api";
import type { WhatsappContent, WhatsappConversationDetail, WhatsappMessage, WhatsappTemplate } from "./types";

/** Inbox → WhatsApp: messages people send to your WhatsApp number, and your replies. */
export function WhatsappInboxPage() {
  const { id = "" } = useParams();
  const [status, setStatus] = useState<"open" | "closed">("open");
  const canRead = useCan("profiles.read");
  return (
    <Page wide>
      <PageHeader
        title="Inbox"
        description="WhatsApp messages people send to your business number, and your replies."
        actions={
          <>
            <ButtonLink to="/inbox" icon={<MessageSquare className="size-3.5" />}>
              Texts
            </ButtonLink>
            <ButtonLink to="/settings/whatsapp" icon={<Settings2 className="size-3.5" />}>
              WhatsApp settings
            </ButtonLink>
          </>
        }
      />
      {!canRead ? (
        <Notice tone="gray" title="Your role can't see messages">
          Messages are people's own words, so only roles that can see profiles can open the inbox. Ask an owner or admin if you need it.
        </Notice>
      ) : (
        <div className="grid grid-cols-1 gap-4 lg:h-[calc(100dvh-9.5rem)] lg:min-h-[520px] lg:grid-cols-[340px_minmax(0,1fr)]">
          <section className={cx("min-h-0 flex-col rounded-lg border border-zinc-200 bg-white shadow-xs", id ? "hidden lg:flex" : "flex")}>
            <div className="flex items-center justify-between gap-2 border-b border-zinc-100 p-3">
              <span className="flex items-center gap-1.5 text-sm font-medium text-zinc-800">
                <MessageCircle className="size-4 text-emerald-600" /> WhatsApp
              </span>
              <Segmented
                size="sm"
                value={status}
                onChange={setStatus}
                options={[
                  { value: "open", label: "Open" },
                  { value: "closed", label: "Closed" },
                ]}
              />
            </div>
            <ConversationList status={status} selected={id} />
          </section>
          <section className={cx("min-h-0", id ? "block" : "hidden lg:block")}>
            {id ? (
              <Thread key={id} id={id} />
            ) : (
              <div className="flex h-full items-center justify-center rounded-lg border border-dashed border-zinc-200 bg-white">
                <EmptyState icon={<MessageCircle className="size-5" />} title="Pick a conversation" description="Choose someone on the left to read their messages and reply." />
              </div>
            )}
          </section>
        </div>
      )}
    </Page>
  );
}

function ConversationList({ status, selected }: { status: "open" | "closed"; selected: string }) {
  const list = useWhatsappConversations(status);
  const markRead = useMarkWhatsappRead();
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
  const rows = list.data.conversations;
  if (rows.length === 0) {
    return (
      <EmptyState
        icon={<MessageCircle className="size-5" />}
        title={status === "open" ? "No WhatsApp messages yet" : "No closed conversations"}
        description={
          status === "open" ? (
            <>
              When someone messages your WhatsApp number, it shows up here. Connect it in{" "}
              <Link to="/settings/whatsapp" className="text-accent-700 underline">
                Settings → WhatsApp
              </Link>
              .
            </>
          ) : (
            "Conversations you close show here until they write again."
          )
        }
      />
    );
  }
  return (
    <ul className="min-h-0 flex-1 divide-y divide-zinc-100 overflow-y-auto" aria-label="WhatsApp conversations">
      {rows.map((c) => (
        <li key={c.id}>
          <button
            type="button"
            onClick={() => {
              if (c.unread_count > 0) markRead.mutate(c.id);
              navigate(`/inbox/whatsapp/${c.id}`);
            }}
            className={cx("flex w-full items-start gap-3 px-3 py-2.5 text-left transition-colors hover:bg-zinc-50", c.id === selected && "bg-accent-50/60 hover:bg-accent-50")}
            aria-current={c.id === selected ? "true" : undefined}
          >
            <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-emerald-50 text-xs font-semibold text-emerald-700">
              {(c.name ?? "#").slice(0, 1).toUpperCase()}
            </span>
            <span className="min-w-0 flex-1">
              <span className="flex items-baseline justify-between gap-2">
                <span className={cx("truncate text-sm", c.unread_count > 0 ? "font-semibold text-zinc-950" : "font-medium text-zinc-800")}>{c.name || formatPhone(c.phone_number)}</span>
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
    </ul>
  );
}

function BackLink() {
  return (
    <Link to="/inbox/whatsapp" className="inline-flex items-center gap-1 text-xs text-zinc-500 hover:text-zinc-900 lg:hidden">
      <ArrowLeft className="size-3.5" /> All conversations
    </Link>
  );
}

const CONSENT: Record<string, { label: string; tone: "green" | "red" | "gray" }> = {
  subscribed: { label: "Subscribed to WhatsApp", tone: "green" },
  unsubscribed: { label: "Opted out of WhatsApp", tone: "red" },
  never_subscribed: { label: "No WhatsApp opt-in", tone: "gray" },
};

function Thread({ id }: { id: string }) {
  const detail = useWhatsappConversation(id);
  const setStatus = useSetWhatsappStatus(id);
  const canSend = useCan("campaigns.send");
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
  const d = detail.data;
  const closed = d.status === "closed";
  const consent = CONSENT[d.contact.consent] ?? CONSENT.never_subscribed;
  return (
    <div className="flex h-full min-h-[70dvh] flex-col rounded-lg border border-zinc-200 bg-white shadow-xs lg:min-h-0">
      <header className="flex flex-wrap items-center justify-between gap-2 border-b border-zinc-100 px-4 py-3">
        <div className="min-w-0">
          <BackLink />
          <h2 className="truncate text-sm font-semibold text-zinc-950">{d.name || formatPhone(d.phone_number)}</h2>
          <div className="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
            <span className="tabular-nums">{formatPhone(d.phone_number)}</span>
            <Badge tone={consent.tone}>{consent.label}</Badge>
            {!d.contact.on_whatsapp ? <Badge tone="red">Not on WhatsApp</Badge> : null}
          </div>
        </div>
        <div className="flex items-center gap-2">
          <Link to={`/audience/profiles/${d.profile_id}`} className="inline-flex items-center gap-1 text-xs font-medium text-zinc-600 hover:text-zinc-900">
            <User className="size-3.5" /> Profile
          </Link>
          {canSend ? (
            <Button
              size="sm"
              variant={closed ? "secondary" : "primary"}
              icon={closed ? <RotateCcw className="size-3.5" /> : <CheckCircle2 className="size-3.5" />}
              loading={setStatus.isPending}
              onClick={() => setStatus.mutate(closed ? "open" : "closed", { onSuccess: () => toast.success(closed ? "Reopened" : "Closed") })}
            >
              {closed ? "Reopen" : "Close"}
            </Button>
          ) : null}
        </div>
      </header>
      <div className="flex min-h-0 flex-1 flex-col-reverse overflow-y-auto bg-[#efeae2]/40 p-4">
        <ul className="space-y-2">
          {d.messages.map((m) => (
            <Bubble key={m.id} m={m} />
          ))}
        </ul>
      </div>
      {canSend ? <Reply id={id} detail={d} /> : <p className="border-t border-zinc-100 p-3 text-xs text-zinc-500">Your role can read messages but not reply.</p>}
    </div>
  );
}

function Bubble({ m }: { m: WhatsappMessage }) {
  const { account } = useSession();
  const out = m.direction === "outbound";
  const label = out
    ? [m.campaign_id ? "Campaign" : m.flow_id ? "Flow" : m.kind === "reply" ? "Reply" : m.kind === "marketing" ? null : "Automatic", m.template ? `template ${m.template}` : null, m.status]
        .filter(Boolean)
        .join(" · ")
    : m.kind === "stop"
      ? "Opted out"
      : m.keyword
        ? `Keyword ${m.keyword.toUpperCase()}`
        : null;
  return (
    <li className={cx("flex", out ? "justify-end" : "justify-start")}>
      <div className={cx("max-w-[80%] rounded-lg px-3 py-2 shadow-sm", out ? "rounded-tr-none bg-emerald-50" : "rounded-tl-none bg-white")}>
        <p className="whitespace-pre-wrap break-words text-sm text-zinc-900">{m.body || (m.type ? `Sent a ${m.type}` : "")}</p>
        <p className="mt-1 text-[11px] text-zinc-500">
          {formatDateTime(m.at, account.timezone)}
          {label ? ` · ${label}` : ""}
        </p>
        {m.error ? <p className="mt-0.5 text-[11px] text-red-600">{m.error}</p> : null}
      </div>
    </li>
  );
}

function Reply({ id, detail }: { id: string; detail: WhatsappConversationDetail }) {
  const reply = useWhatsappReply(id);
  const [text, setText] = useState("");
  const [template, setTemplate] = useState<WhatsappContent>({ template_id: null, body: {} });
  const free = detail.reply.free_form;
  const templates = detail.reply.templates.filter((t) => detail.contact.consent === "subscribed" || t.category !== "MARKETING");
  const chosen = templates.find((t) => t.id === template.template_id) ?? null;
  const done = (r: { sent: boolean; reason?: string }) => {
    if (r.sent) {
      setText("");
      setTemplate({ template_id: null, body: {} });
      toast.success("Sent");
    } else toast.error(r.reason ?? "Not sent");
  };
  return (
    <div className="space-y-2 border-t border-zinc-100 p-3">
      {free ? (
        <div className="flex gap-2">
          <Textarea rows={2} value={text} onChange={(e) => setText(e.target.value)} placeholder="Write a reply…" aria-label="Reply" maxLength={4096} />
          <Button variant="primary" icon={<Send className="size-3.5" />} disabled={!text.trim()} loading={reply.isPending} onClick={() => reply.mutate({ text }, { onSuccess: done })}>
            Send
          </Button>
        </div>
      ) : (
        <>
          <p className="text-xs text-zinc-500">{detail.reply.why}</p>
          {detail.contact.consent === "unsubscribed" ? (
            <p className="text-xs text-zinc-500">They opted out of WhatsApp messages from you, so no template can go.</p>
          ) : (
            <TemplateReply templates={templates} chosen={chosen} value={template} onChange={setTemplate} />
          )}
          {chosen ? (
            <Button variant="primary" icon={<Send className="size-3.5" />} loading={reply.isPending} onClick={() => reply.mutate({ template }, { onSuccess: done })}>
              Send template
            </Button>
          ) : null}
        </>
      )}
      {reply.error ? <InlineError message={reply.error.message} /> : null}
    </div>
  );
}

function TemplateReply({ templates, chosen, value, onChange }: { templates: WhatsappTemplate[]; chosen: WhatsappTemplate | null; value: WhatsappContent; onChange: (v: WhatsappContent) => void }) {
  if (templates.length === 0) return <p className="text-xs text-zinc-500">No approved template can go to them. Templates are in Settings → WhatsApp.</p>;
  const v = chosen?.variables;
  return (
    <div className="space-y-2">
      <Select value={value.template_id ?? ""} onChange={(e) => onChange({ template_id: e.target.value || null, body: {} })} aria-label="Template">
        <option value="">Pick an approved template…</option>
        {templates.map((t) => (
          <option key={t.id} value={t.id}>
            {t.name} ({t.language})
          </option>
        ))}
      </Select>
      {v ? (
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
          {v.header ? (
            <Field label="Header">
              <Input value={value.header ?? ""} onChange={(e) => onChange({ ...value, header: e.target.value })} />
            </Field>
          ) : null}
          {v.header_media ? (
            <Field label={`Header ${v.header_media.toLowerCase()} link`}>
              <Input value={value.header_media_url ?? ""} onChange={(e) => onChange({ ...value, header_media_url: e.target.value })} placeholder="https://…" />
            </Field>
          ) : null}
          {v.body.map((n) => (
            <Field key={n} label={`{{${n}}}`}>
              <Input value={value.body[n] ?? ""} onChange={(e) => onChange({ ...value, body: { ...value.body, [n]: e.target.value } })} />
            </Field>
          ))}
          {v.url_buttons.map((i) => (
            <Field key={i} label={`Button ${i + 1} link ending`}>
              <Input value={value.buttons?.[String(i)] ?? ""} onChange={(e) => onChange({ ...value, buttons: { ...(value.buttons ?? {}), [String(i)]: e.target.value } })} />
            </Field>
          ))}
        </div>
      ) : null}
    </div>
  );
}

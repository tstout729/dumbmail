import { useState, type FormEvent } from "react";
import { useNavigate, useSearchParams } from "react-router";
import { Archive, ArchiveRestore, CalendarDays, Copy, Mail, MessageSquare, MoreHorizontal, Plus, Search, Tag, Trash2 } from "lucide-react";
import type { CampaignStatus, Channel } from "../../api";
import { useCreateCampaign, useDeleteCampaign, useDuplicateCampaign, useSession } from "../../api";
import { starterEmail } from "../../components/email/blocks";
import {
  Badge,
  Button,
  ButtonLink,
  ConfirmDialog,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  Input,
  Menu,
  Modal,
  Page,
  PageHeader,
  RowLink,
  Select,
  SkeletonRows,
  StatusBadge,
  Table,
  Tabs,
  Td,
  Th,
  cx,
} from "../../components/ui";
import { formatCurrency, formatDateTime, formatNumber, formatPercent } from "../../lib/format";
import { useApplyTags, useArchive, useCampaignCounts, useCampaignSearch, useTags } from "../../features/campaignsPlus/api";
import type { CampaignListItem, CampaignSearch } from "../../features/campaignsPlus/types";

type Tab = "all" | "draft" | "scheduled" | "sent" | "archived";
const tabStatuses: Record<Tab, CampaignStatus[] | null> = {
  all: null,
  draft: ["draft"],
  // In progress counts as scheduled until it's fully sent, including an A/B test waiting on its winner.
  scheduled: ["scheduled", "sending", "testing"],
  sent: ["sent", "cancelled", "failed"],
  archived: null,
};
const PAGE = 50;

export function ChannelIcon({ channel }: { channel: Channel }) {
  return channel === "email" ? (
    <Mail className="size-4 shrink-0 text-zinc-400" aria-label="Email" />
  ) : (
    <MessageSquare className="size-4 shrink-0 text-zinc-400" aria-label="SMS" />
  );
}

export function CampaignsPage() {
  const { account } = useSession();
  const [params, setParams] = useSearchParams();
  const tab = (params.get("tab") as Tab | null) ?? "all";
  const creating = params.get("create") === "1";
  const [q, setQ] = useState(params.get("q") ?? "");
  const tag = params.get("tag") ?? "";
  const channel = (params.get("channel") as Channel | null) ?? undefined;
  const from = params.get("from") ?? "";
  const to = params.get("to") ?? "";
  const offset = Number(params.get("offset") ?? 0) || 0;
  const statuses = tabStatuses[tab];
  const search: CampaignSearch = {
    status: statuses?.join(","),
    archived: tab === "archived" ? "true" : undefined,
    q: params.get("q") ?? undefined,
    tag: tag || undefined,
    channel,
    from: from || undefined,
    to: to || undefined,
    limit: PAGE,
    offset: offset || undefined,
  };
  const campaigns = useCampaignSearch(search);
  const counts = useCampaignCounts();
  const tags = useTags();
  const [deleting, setDeleting] = useState<CampaignListItem | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const duplicate = useDuplicateCampaign();
  const remove = useDeleteCampaign();
  const archive = useArchive("campaigns");
  const navigate = useNavigate();

  const rows = campaigns.data?.data ?? [];
  const total = campaigns.data?.total ?? 0;
  const by = counts.data?.by_status ?? {};
  const sum = (ss: CampaignStatus[]) => ss.reduce((n, s) => n + (by[s] ?? 0), 0);
  const count = (t: Tab) => (t === "archived" ? (counts.data?.archived ?? 0) : sum(tabStatuses[t] ?? (Object.keys(by) as CampaignStatus[])));
  const filtered = Boolean(params.get("q") || tag || channel || from || to);
  const nothingYet = !campaigns.isPending && total === 0 && !filtered && tab === "all";

  const setParam = (patch: Record<string, string | null>) => {
    const next = new URLSearchParams(params);
    for (const [key, value] of Object.entries(patch)) {
      if (value === null || value === "") next.delete(key);
      else next.set(key, value);
    }
    if (!("offset" in patch)) next.delete("offset");
    setParams(next, { replace: true });
    setSelected([]);
  };
  const allOnPage = rows.length > 0 && rows.every((r) => selected.includes(r.id));
  const toggle = (id: string) => setSelected((s) => (s.includes(id) ? s.filter((x) => x !== id) : [...s, id]));

  return (
    <Page>
      <PageHeader
        title="Campaigns"
        description="One-time emails and texts to a list or segment."
        actions={
          <>
            <ButtonLink to="/campaigns/calendar" variant="ghost" icon={<CalendarDays className="size-4" />}>
              Calendar
            </ButtonLink>
            <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => setParam({ create: "1" })}>
              Create campaign
            </Button>
          </>
        }
      />
      <Tabs
        value={tab}
        onChange={(t) => setParam({ tab: t === "all" ? null : t })}
        tabs={[
          { value: "all", label: "All", count: count("all") },
          { value: "draft", label: "Drafts", count: count("draft") },
          { value: "scheduled", label: "Scheduled", count: count("scheduled") },
          { value: "sent", label: "Sent", count: count("sent") },
          { value: "archived", label: "Archived", count: count("archived") },
        ]}
      />
      <form
        className="mb-4 flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          setParam({ q: q.trim() || null });
        }}
      >
        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={q} onChange={(e) => setQ(e.target.value)} onBlur={() => setParam({ q: q.trim() || null })} placeholder="Search name or subject" aria-label="Search campaigns" className="w-56 pl-8" maxLength={200} />
        </div>
        <Select value={tag} onChange={(e) => setParam({ tag: e.target.value || null })} aria-label="Tag" className="w-40">
          <option value="">All tags</option>
          {(tags.data?.data ?? []).map((t) => (
            <option key={t.id} value={t.id}>
              {t.name}
            </option>
          ))}
        </Select>
        <Select value={channel ?? ""} onChange={(e) => setParam({ channel: e.target.value || null })} aria-label="Channel" className="w-40">
          <option value="">Email and SMS</option>
          <option value="email">Email</option>
          <option value="sms">SMS</option>
        </Select>
        <Input type="date" value={from} onChange={(e) => setParam({ from: e.target.value || null })} aria-label="Sent or scheduled from" className="w-38" />
        <span className="pb-2 text-xs text-zinc-400">to</span>
        <Input type="date" value={to} onChange={(e) => setParam({ to: e.target.value || null })} aria-label="Sent or scheduled until" className="w-38" />
        {filtered ? (
          <Button variant="ghost" size="sm" onClick={() => { setQ(""); setParam({ q: null, tag: null, channel: null, from: null, to: null }); }}>
            Clear filters
          </Button>
        ) : null}
      </form>
      {selected.length > 0 ? <BulkBar ids={selected} archived={tab === "archived"} onDone={() => setSelected([])} /> : null}
      <Table>
        <thead>
          <tr>
            <Th className="w-8">
              <input
                type="checkbox"
                aria-label="Select all on this page"
                className="size-4 rounded border-zinc-300 accent-accent-700"
                checked={allOnPage}
                onChange={() => setSelected(allOnPage ? [] : rows.map((r) => r.id))}
              />
            </Th>
            <Th>Campaign</Th>
            <Th>Status</Th>
            <Th>Send date</Th>
            <Th className="text-right">Recipients</Th>
            <Th className="text-right">Open rate</Th>
            <Th className="text-right">Click rate</Th>
            <Th className="text-right">Revenue</Th>
            <Th className="w-10" />
          </tr>
        </thead>
        <tbody>
          {campaigns.isPending ? (
            <SkeletonRows cols={9} />
          ) : campaigns.error ? (
            <EmptyRow cols={9}>
              <p className="text-sm text-red-600">{campaigns.error.message}</p>
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={9}>
              <EmptyState
                icon={<Mail className="size-5" />}
                title={nothingYet ? "No campaigns yet" : "Nothing here"}
                description={nothingYet ? "Send your first email or text to a list or segment." : tab === "archived" ? "Archived campaigns show up here." : "Try another tab, tag or search."}
                action={
                  nothingYet ? (
                    <Button variant="primary" onClick={() => setParam({ create: "1" })}>
                      Create campaign
                    </Button>
                  ) : undefined
                }
              />
            </EmptyRow>
          ) : (
            rows.map((c) => {
              const sent = c.status === "sent" || c.status === "sending" || c.status === "testing";
              const canArchive = c.archived_at || !["scheduled", "sending", "testing"].includes(c.status);
              return (
                <RowLink key={c.id} to={`/campaigns/${c.id}`}>
                  <Td onClick={(e) => e.stopPropagation()}>
                    <input
                      type="checkbox"
                      aria-label={`Select ${c.name}`}
                      className="size-4 rounded border-zinc-300 accent-accent-700"
                      checked={selected.includes(c.id)}
                      onChange={() => toggle(c.id)}
                    />
                  </Td>
                  <Td>
                    <div className="flex items-center gap-2.5">
                      <ChannelIcon channel={c.channel} />
                      <div className="min-w-36">
                        <span className="font-medium text-zinc-900">
                          {c.name}
                          {c.ab_test ? (
                            <span className="ml-2 align-middle">
                              <Badge tone="violet">A/B</Badge>
                            </span>
                          ) : null}
                          {c.resend_of ? (
                            <span className="ml-2 align-middle">
                              <Badge tone="blue">Resend</Badge>
                            </span>
                          ) : null}
                        </span>
                        {c.tags.length > 0 ? (
                          <div className="mt-1 flex flex-wrap gap-1">
                            {c.tags.map((t) => (
                              <button
                                key={t.id}
                                type="button"
                                onClick={(e) => {
                                  e.preventDefault();
                                  e.stopPropagation();
                                  setParam({ tag: t.id });
                                }}
                                className="rounded bg-zinc-100 px-1.5 py-0.5 text-[11px] text-zinc-700 hover:bg-zinc-200"
                              >
                                {t.name}
                              </button>
                            ))}
                          </div>
                        ) : null}
                      </div>
                    </div>
                  </Td>
                  <Td>
                    <StatusBadge status={c.status} />
                  </Td>
                  <Td className="whitespace-nowrap text-zinc-600">{formatDateTime(c.sent_at ?? c.send_at, account.timezone)}</Td>
                  <Td className="text-right tabular-nums">{sent ? formatNumber(c.stats.recipients) : "—"}</Td>
                  <Td className="text-right tabular-nums">{sent && c.channel === "email" ? formatPercent(c.stats.open_rate) : "—"}</Td>
                  <Td className="text-right tabular-nums">{sent ? formatPercent(c.stats.click_rate) : "—"}</Td>
                  <Td className="text-right font-medium tabular-nums">{sent ? formatCurrency(c.stats.revenue) : "—"}</Td>
                  <Td>
                    <Menu
                      trigger={(props) => (
                        <IconButton label="Campaign actions" {...props}>
                          <MoreHorizontal className="size-4" />
                        </IconButton>
                      )}
                      items={[
                        {
                          label: "Duplicate",
                          icon: <Copy className="size-3.5" />,
                          onSelect: () => duplicate.mutate(c.id, { onSuccess: (dup) => navigate(`/campaigns/${dup.id}`) }),
                        },
                        {
                          label: c.archived_at ? "Unarchive" : "Archive",
                          icon: c.archived_at ? <ArchiveRestore className="size-3.5" /> : <Archive className="size-3.5" />,
                          disabled: !canArchive,
                          onSelect: () => archive.mutate({ ids: [c.id], archived: !c.archived_at }),
                        },
                        {
                          label: "Delete",
                          icon: <Trash2 className="size-3.5" />,
                          danger: true,
                          disabled: !(c.status === "draft" || c.status === "scheduled"),
                          onSelect: () => setDeleting(c),
                        },
                      ]}
                    />
                  </Td>
                </RowLink>
              );
            })
          )}
        </tbody>
      </Table>
      {total > PAGE ? (
        <div className="mt-3 flex items-center justify-between text-sm text-zinc-600">
          <span className="tabular-nums">
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + PAGE, total))} of {formatNumber(total)}
          </span>
          <div className="flex gap-2">
            <Button size="sm" disabled={offset === 0} onClick={() => setParam({ offset: String(Math.max(0, offset - PAGE)) })}>
              Previous
            </Button>
            <Button size="sm" disabled={offset + PAGE >= total} onClick={() => setParam({ offset: String(offset + PAGE) })}>
              Next
            </Button>
          </div>
        </div>
      ) : null}
      {creating ? <CreateCampaignDialog onClose={() => setParam({ create: null })} /> : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete "${deleting.name}"?`}
          body="This can't be undone."
          confirmLabel="Delete"
          danger
          onConfirm={() => {
            remove.mutate(deleting.id);
            setDeleting(null);
          }}
          onClose={() => setDeleting(null)}
        />
      ) : null}
    </Page>
  );
}

/** Actions on the selected campaigns: add or remove a tag, archive or unarchive. */
function BulkBar({ ids, archived, onDone }: { ids: string[]; archived: boolean; onDone: () => void }) {
  const tags = useTags();
  const apply = useApplyTags("campaigns");
  const archive = useArchive("campaigns");
  const [name, setName] = useState("");
  const [removing, setRemoving] = useState("");
  return (
    <div className="mb-3 flex flex-wrap items-center gap-2 rounded-lg border border-accent-200 bg-accent-50/40 px-3 py-2 text-sm">
      <span className="font-medium text-zinc-800">{ids.length} selected</span>
      <form
        className="flex items-center gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          if (name.trim()) apply.mutate({ ids, add: [name.trim()] }, { onSuccess: () => setName("") });
        }}
      >
        <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="Tag name" aria-label="Tag to add" maxLength={40} list="campaign-tag-names" className="h-8 w-36" />
        <datalist id="campaign-tag-names">
          {(tags.data?.data ?? []).map((t) => (
            <option key={t.id} value={t.name} />
          ))}
        </datalist>
        <Button size="sm" type="submit" icon={<Tag className="size-3.5" />} loading={apply.isPending} disabled={!name.trim()}>
          Add tag
        </Button>
      </form>
      {(tags.data?.data ?? []).length > 0 ? (
        <div className="flex items-center gap-1">
          <Select value={removing} onChange={(e) => setRemoving(e.target.value)} aria-label="Tag to remove" className="h-8 w-36">
            <option value="">Remove a tag…</option>
            {(tags.data?.data ?? []).map((t) => (
              <option key={t.id} value={t.name}>
                {t.name}
              </option>
            ))}
          </Select>
          <Button size="sm" disabled={!removing} onClick={() => apply.mutate({ ids, remove: [removing] }, { onSuccess: () => setRemoving("") })}>
            Remove
          </Button>
        </div>
      ) : null}
      <Button size="sm" icon={archived ? <ArchiveRestore className="size-3.5" /> : <Archive className="size-3.5" />} loading={archive.isPending} onClick={() => archive.mutate({ ids, archived: !archived }, { onSuccess: onDone })}>
        {archived ? "Unarchive" : "Archive"}
      </Button>
      <button type="button" onClick={onDone} className={cx("ml-auto text-xs text-zinc-500 hover:text-zinc-800")}>
        Clear selection
      </button>
      {apply.error ? <p className="w-full text-xs text-red-600">{apply.error.message}</p> : null}
    </div>
  );
}

function CreateCampaignDialog({ onClose }: { onClose: () => void }) {
  const { account } = useSession();
  const [channel, setChannel] = useState<Channel>("email");
  const create = useCreateCampaign();
  const navigate = useNavigate();

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate(
      {
        name,
        channel,
        audience: { include: [], exclude: [] },
        content: channel === "email" ? starterEmail(account.brand_color) : { body: "" },
      },
      { onSuccess: (c) => navigate(`/campaigns/${c.id}`) },
    );
  };

  const defaultName = `${new Intl.DateTimeFormat("en-US", { timeZone: account.timezone, month: "short", day: "numeric" }).format(new Date())} campaign`;

  return (
    <Modal title="Create campaign" onClose={onClose} size="md">
      <form onSubmit={onSubmit} className="space-y-4">
        <div className="grid grid-cols-2 gap-3">
          {(
            [
              ["email", Mail, "Email", "Newsletters, promos, launches"],
              ["sms", MessageSquare, "SMS", "Short, urgent, high-intent"],
            ] as const
          ).map(([value, Icon, label, detail]) => (
            <button
              key={value}
              type="button"
              onClick={() => setChannel(value)}
              className={cx(
                "rounded-lg border p-4 text-left transition-colors",
                channel === value ? "border-accent-500 bg-accent-50/50 ring-1 ring-accent-500" : "border-zinc-200 hover:border-zinc-300",
              )}
              aria-pressed={channel === value}
            >
              <Icon className={cx("size-5", channel === value ? "text-accent-700" : "text-zinc-400")} />
              <div className="mt-2 text-sm font-semibold text-zinc-900">{label}</div>
              <div className="text-xs text-zinc-500">{detail}</div>
            </button>
          ))}
        </div>
        <Field label="Campaign name" hint="Only you see this.">
          <Input name="name" required autoFocus defaultValue={defaultName} />
        </Field>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Create and edit
          </Button>
        </div>
      </form>
    </Modal>
  );
}

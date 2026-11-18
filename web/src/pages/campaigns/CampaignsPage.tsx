import { useState, type FormEvent } from "react";
import { useNavigate, useSearchParams } from "react-router";
import { Copy, Mail, MessageSquare, MoreHorizontal, Plus, Search, Trash2, BellRing } from "lucide-react";
import type { Campaign, CampaignStatus, Channel } from "../../api";
import { useCampaigns, useCreateCampaign, useDeleteCampaign, useDuplicateCampaign, useSession } from "../../api";
import { starterEmail } from "../../components/email/blocks";
import {
  Badge,
  Button,
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
  SkeletonRows,
  StatusBadge,
  Table,
  Tabs,
  Td,
  Th,
  cx,
} from "../../components/ui";
import { formatCurrency, formatDateTime, formatNumber, formatPercent } from "../../lib/format";

type Tab = "all" | "draft" | "scheduled" | "sent";
const tabStatuses: Record<Tab, CampaignStatus[] | null> = {
  all: null,
  draft: ["draft"],
  // In progress counts as scheduled until it's fully sent, including an A/B test waiting on its winner.
  scheduled: ["scheduled", "sending", "testing"],
  sent: ["sent", "cancelled", "failed"],
};

export function ChannelIcon({ channel }: { channel: Channel }) {
  if (channel === "push") return <BellRing className="size-4 shrink-0 text-zinc-400" aria-label="Push" />;
  return channel === "email" ? (
    <Mail className="size-4 shrink-0 text-zinc-400" aria-label="Email" />
  ) : (
    <MessageSquare className="size-4 shrink-0 text-zinc-400" aria-label="SMS" />
  );
}

export function CampaignsPage() {
  const { account } = useSession();
  const campaigns = useCampaigns();
  const [params, setParams] = useSearchParams();
  const tab = (params.get("tab") as Tab | null) ?? "all";
  const creating = params.get("create") === "1";
  const [q, setQ] = useState("");
  const [deleting, setDeleting] = useState<Campaign | null>(null);
  const duplicate = useDuplicateCampaign();
  const remove = useDeleteCampaign();
  const navigate = useNavigate();

  const all = campaigns.data ?? [];
  const statuses = tabStatuses[tab];
  const rows = all
    .filter((c) => !statuses || statuses.includes(c.status))
    .filter((c) => c.name.toLowerCase().includes(q.toLowerCase()))
    .sort((a, b) => (b.sent_at ?? b.send_at ?? b.updated_at).localeCompare(a.sent_at ?? a.send_at ?? a.updated_at));
  const count = (t: Tab) => (tabStatuses[t] ? all.filter((c) => tabStatuses[t]?.includes(c.status)).length : all.length);

  const setParam = (key: string, value: string | null) => {
    const next = new URLSearchParams(params);
    if (value === null) next.delete(key);
    else next.set(key, value);
    setParams(next, { replace: true });
  };

  return (
    <Page>
      <PageHeader
        title="Campaigns"
        description="One-time emails and texts to a list or segment."
        actions={
          <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => setParam("create", "1")}>
            Create campaign
          </Button>
        }
      />
      <div className="flex flex-wrap items-end justify-between gap-3">
        <Tabs
          value={tab}
          onChange={(t) => setParam("tab", t === "all" ? null : t)}
          tabs={[
            { value: "all", label: "All", count: count("all") },
            { value: "draft", label: "Drafts", count: count("draft") },
            { value: "scheduled", label: "Scheduled", count: count("scheduled") },
            { value: "sent", label: "Sent", count: count("sent") },
          ]}
        />
        <div className="relative mb-4">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search campaigns" className="w-56 pl-8" />
        </div>
      </div>
      <Table>
        <thead>
          <tr>
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
            <SkeletonRows cols={8} />
          ) : rows.length === 0 ? (
            <EmptyRow cols={8}>
              <EmptyState
                icon={<Mail className="size-5" />}
                title={all.length === 0 ? "No campaigns yet" : "Nothing here"}
                description={all.length === 0 ? "Send your first email or text to a list or segment." : "Try another tab or search."}
                action={
                  all.length === 0 ? (
                    <Button variant="primary" onClick={() => setParam("create", "1")}>
                      Create campaign
                    </Button>
                  ) : undefined
                }
              />
            </EmptyRow>
          ) : (
            rows.map((c) => {
              const sent = c.status === "sent" || c.status === "sending" || c.status === "testing";
              return (
                <RowLink key={c.id} to={`/campaigns/${c.id}`}>
                  <Td>
                    <div className="flex items-center gap-2.5">
                      <ChannelIcon channel={c.channel} />
                      <span className="min-w-36 font-medium text-zinc-900">
                        {c.name}
                        {c.ab_test ? (
                          <span className="ml-2 align-middle">
                            <Badge tone="violet">A/B</Badge>
                          </span>
                        ) : null}
                      </span>
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
      {creating ? <CreateCampaignDialog onClose={() => setParam("create", null)} /> : null}
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
        content: channel === "email" ? starterEmail(account.brand_color) : channel === "push" ? { title: "", body: "" } : { body: "" },
      },
      { onSuccess: (c) => navigate(`/campaigns/${c.id}`) },
    );
  };

  const defaultName = `${new Intl.DateTimeFormat("en-US", { timeZone: account.timezone, month: "short", day: "numeric" }).format(new Date())} campaign`;

  return (
    <Modal title="Create campaign" onClose={onClose} size="md">
      <form onSubmit={onSubmit} className="space-y-4">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
          {(
            [
              ["email", Mail, "Email", "Newsletters, promos, launches"],
              ["sms", MessageSquare, "SMS", "Short, urgent, high-intent"],
              ["push", BellRing, "Push", "Browser and app notifications"],
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

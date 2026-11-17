import { useState, type FormEvent, type ReactNode } from "react";
import { ChevronRight, Copy, ExternalLink, Plus, RefreshCw, Trash2 } from "lucide-react";
import type { DnsPurpose, DnsRecord, Sending, SendingDomain } from "../../api";
import {
  useAddDomain,
  useRemoveDomain,
  useRemoveLinkDomain,
  useSending,
  useSession,
  useSetLinkDomain,
  useVerifyDomain,
  useVerifyLinkDomain,
} from "../../api";
import { Badge, Button, Card, ConfirmDialog, Input, Skeleton, StatusBadge, Table, Td, Th, cx } from "../../components/ui";
import { copyText } from "../../lib/clipboard";
import { toast } from "../../lib/toast";
import { hostnameOf } from "./shared";

const purposeLabels: Record<DnsPurpose, string> = {
  dkim: "DKIM signature",
  spf: "Return path (SPF)",
  mail_from: "Return path (MX)",
  dmarc: "DMARC policy",
  link: "Branded links",
  verification: "Ownership",
};

function cleanDomain(raw: FormDataEntryValue | null): string {
  return String(raw ?? "")
    .trim()
    .toLowerCase()
    .replace(/^https?:\/\//, "")
    .replace(/\/.*$/, "");
}

function notReadyMessage(records: DnsRecord[]): string {
  const missing = records.filter((r) => r.status === "missing" || r.status === "pending").length;
  return missing
    ? `${missing} ${missing === 1 ? "record isn't" : "records aren't"} showing up yet. DNS changes can take up to an hour.`
    : "Not verified yet. DNS changes can take up to an hour.";
}

/** "6 of 7 records found", counting only the records that are required. */
function recordSummary(records: DnsRecord[]): string {
  const required = records.filter((r) => r.purpose !== "dmarc");
  const found = required.filter((r) => r.status === "verified").length;
  return `${found} of ${required.length} records found`;
}

export function DomainsPage() {
  const sending = useSending();
  if (sending.isPending) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-48" />
        <Skeleton className="h-28" />
      </div>
    );
  }
  if (sending.error) return <p className="text-sm text-red-600">{sending.error.message}</p>;
  return (
    <div className="space-y-4">
      <SendingDomains sending={sending.data} />
      <LinkDomain sending={sending.data} />
    </div>
  );
}

// ---------- Sending domains ----------

function SendingDomains({ sending }: { sending: Sending }) {
  const { account } = useSession();
  const add = useAddDomain();
  const { domains, max_domains: max } = sending.email;
  const [adding, setAdding] = useState(domains.length === 0);
  // Domains that still need DNS work start open; verified ones stay out of the way.
  const [open, setOpen] = useState(() => new Set(domains.filter((d) => d.status !== "verified").map((d) => d.domain)));
  const toggle = (domain: string) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(domain)) next.delete(domain);
      else next.add(domain);
      return next;
    });

  const taken = new Set(domains.map((d) => d.domain));
  const fromDomain = account.from_email?.split("@")[1];
  const suggestion = [fromDomain, hostnameOf(account.website)].find((d) => d && !taken.has(d)) ?? "";

  const onAdd = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const domain = cleanDomain(new FormData(e.currentTarget).get("domain"));
    add.mutate(domain, {
      onSuccess: (r) => {
        setAdding(false);
        const added = r.email.domains.find((d) => d.domain === domain);
        if (added?.status === "verified") toast.success(`${domain} is verified`);
        else setOpen((prev) => new Set(prev).add(domain));
      },
    });
  };

  return (
    <Card
      title="Sending domains"
      padded={false}
      actions={
        !adding && domains.length < max ? (
          <Button size="sm" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
            Add domain
          </Button>
        ) : null
      }
    >
      <p className="px-4 pt-3 text-sm text-zinc-600">
        You can send from any address on a verified domain. Gmail and Yahoo reject or spam-folder bulk mail without these records.
      </p>
      {adding ? (
        <form onSubmit={onAdd} className="flex flex-wrap gap-2 px-4 pt-3">
          <Input name="domain" required autoFocus defaultValue={suggestion} placeholder="yourbrand.com" className="max-w-xs" aria-label="Domain to send from" />
          <Button type="submit" variant="primary" loading={add.isPending}>
            Get DNS records
          </Button>
          {domains.length > 0 ? (
            <Button variant="ghost" onClick={() => setAdding(false)}>
              Cancel
            </Button>
          ) : null}
        </form>
      ) : null}
      <ul className="mt-3 divide-y divide-zinc-100 border-t border-zinc-100">
        {domains.map((d) => (
          <SendingDomainRow key={d.domain} domain={d} open={open.has(d.domain)} onToggle={() => toggle(d.domain)} />
        ))}
        {domains.length === 0 ? <li className="px-4 py-3 text-sm text-zinc-500">No sending domains yet.</li> : null}
      </ul>
    </Card>
  );
}

function SendingDomainRow({ domain: d, open, onToggle }: { domain: SendingDomain; open: boolean; onToggle: () => void }) {
  const verify = useVerifyDomain();
  const remove = useRemoveDomain();
  const [confirmRemove, setConfirmRemove] = useState(false);
  const verified = d.status === "verified";

  return (
    <li>
      <DomainSummary
        name={d.domain}
        open={open}
        onToggle={onToggle}
        tags={d.default_sender ? <Badge>Default sender</Badge> : null}
        detail={verified ? "Signed and aligned for inbox placement" : recordSummary(d.dns_records)}
        status={<StatusBadge status={d.status} />}
      />
      {open ? (
        <DomainDetails
          records={d.dns_records}
          zone={d.domain}
          verified={verified}
          note={d.mail_from_domain ? <>Bounces and complaints return to {d.mail_from_domain}.</> : null}
          checking={verify.isPending}
          onCheck={() =>
            verify.mutate(d.domain, {
              onSuccess: (r) => {
                const now = r.email.domains.find((x) => x.domain === d.domain);
                if (now?.status === "verified") toast.success(`${d.domain} is verified`);
                else toast.info(notReadyMessage(now?.dns_records ?? []));
              },
            })
          }
          onRemove={() => setConfirmRemove(true)}
        />
      ) : null}
      {confirmRemove ? (
        <ConfirmDialog
          title={`Remove ${d.domain}?`}
          body={
            d.default_sender
              ? `Your default sender email is on ${d.domain}, so emails stop sending until you verify it again or change the sender in Settings → Sending.`
              : `Emails from addresses on ${d.domain} stop sending. You can add it again later; the DNS records stay the same.`
          }
          confirmLabel="Remove domain"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(d.domain, { onSuccess: () => setConfirmRemove(false) })}
          onClose={() => setConfirmRemove(false)}
        />
      ) : null}
    </li>
  );
}

// ---------- Branded link domain ----------

function LinkDomain({ sending }: { sending: Sending }) {
  const link = sending.link_domain;
  const set = useSetLinkDomain();
  const verify = useVerifyLinkDomain();
  const remove = useRemoveLinkDomain();
  const [open, setOpen] = useState(link.status !== "active");
  const [confirmRemove, setConfirmRemove] = useState(false);
  const base = sending.email.domains.find((d) => d.status === "verified")?.domain ?? sending.email.domains[0]?.domain;

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    set.mutate(cleanDomain(new FormData(e.currentTarget).get("domain")), { onSuccess: () => setOpen(true) });
  };

  return (
    <Card title={<span className="flex items-center gap-2">Branded link domain <Badge>Optional</Badge></span>} padded={false}>
      <p className="px-4 pt-3 text-sm text-zinc-600">
        Tracking links, unsubscribe pages and sign-up forms use your own domain instead of DumbMail&apos;s, which looks more trustworthy.
      </p>
      {!link.domain ? (
        <form onSubmit={onSubmit} className="flex flex-wrap gap-2 px-4 py-3">
          <Input name="domain" required defaultValue={base ? `links.${base}` : ""} placeholder="links.yourbrand.com" className="max-w-xs" aria-label="Link domain" />
          <Button type="submit" loading={set.isPending}>
            Get DNS records
          </Button>
        </form>
      ) : (
        <div className="mt-3 border-t border-zinc-100">
          <DomainSummary
            name={link.domain}
            open={open}
            onToggle={() => setOpen((o) => !o)}
            detail={
              link.url ? (
                <a href={link.url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-0.5 hover:text-zinc-800" onClick={(e) => e.stopPropagation()}>
                  Links use {link.url} <ExternalLink className="size-3" />
                </a>
              ) : (
                recordSummary(link.dns_records)
              )
            }
            status={<StatusBadge status={link.status} />}
          />
          {open ? (
            <DomainDetails
              records={link.dns_records}
              zone={link.domain.split(".").slice(1).join(".")}
              verified={link.status === "active"}
              checking={verify.isPending}
              onCheck={() =>
                verify.mutate(undefined, {
                  onSuccess: (r) =>
                    r.link_domain.status === "active" ? toast.success("Link domain is active") : toast.info(notReadyMessage(r.link_domain.dns_records)),
                })
              }
              onRemove={() => setConfirmRemove(true)}
            />
          ) : null}
        </div>
      )}
      {confirmRemove && link.domain ? (
        <ConfirmDialog
          title={`Remove ${link.domain}?`}
          body="New links go back to the default DumbMail domain. Links in emails you already sent keep working only while the DNS record stays in place."
          confirmLabel="Remove domain"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(undefined, { onSuccess: () => setConfirmRemove(false) })}
          onClose={() => setConfirmRemove(false)}
        />
      ) : null}
    </Card>
  );
}

// ---------- Shared pieces ----------

type DomainSummaryProps = { name: string; open: boolean; onToggle: () => void; tags?: ReactNode; detail: ReactNode; status: ReactNode };

/** One clickable line per domain: name, a short status line and the badge. */
function DomainSummary({ name, open, onToggle, tags, detail, status }: DomainSummaryProps) {
  return (
    <div
      role="button"
      tabIndex={0}
      aria-expanded={open}
      onClick={onToggle}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onToggle();
        }
      }}
      className="flex cursor-pointer items-center gap-3 px-4 py-3 hover:bg-zinc-50"
    >
      <ChevronRight className={cx("size-4 shrink-0 text-zinc-400 transition-transform", open && "rotate-90")} />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="truncate text-sm font-semibold text-zinc-900">{name}</span>
          {tags}
        </div>
        <div className="text-xs text-zinc-500">{detail}</div>
      </div>
      {status}
    </div>
  );
}

type DomainDetailsProps = {
  records: DnsRecord[];
  /** The DNS zone the records live in, for the "enter only the host part" hint. */
  zone: string;
  verified: boolean;
  note?: ReactNode;
  checking: boolean;
  onCheck: () => void;
  onRemove: () => void;
};

function DomainDetails({ records, zone, verified, note, checking, onCheck, onRemove }: DomainDetailsProps) {
  return (
    <div className="space-y-3 bg-zinc-50/60 px-4 pb-4 pt-1">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="max-w-xl text-xs text-zinc-500">
          {verified ? (
            note ?? "Everything is in place."
          ) : (
            <>
              Add these at your DNS host (Cloudflare, GoDaddy, Namecheap…), then check. If your host adds the domain for you, enter only the
              part of the name before <code>.{zone}</code>.
            </>
          )}
        </p>
        <div className="flex gap-2">
          <Button variant="ghost" size="sm" icon={<Trash2 className="size-3.5" />} onClick={onRemove}>
            Remove
          </Button>
          <Button size="sm" icon={<RefreshCw className="size-3.5" />} loading={checking} onClick={onCheck}>
            Check DNS
          </Button>
        </div>
      </div>
      <DnsRecordsTable records={records} />
    </div>
  );
}

function FoundNote({ r }: { r: DnsRecord }) {
  if (r.status === "verified" || r.found === undefined) return null;
  return (
    <div className="mt-1 text-[11px] text-zinc-500">
      {r.found ? (
        <>
          DNS shows: <code className="break-all font-mono text-amber-700">{r.found}</code>
        </>
      ) : (
        "Nothing found in public DNS yet"
      )}
    </div>
  );
}

function RecordStatus({ r }: { r: DnsRecord }) {
  return <StatusBadge status={r.status} label={r.status === "recommended" ? "Optional" : undefined} />;
}

/** A table on wide screens; one block per record on phones, so the long values stay readable. */
function DnsRecordsTable({ records }: { records: DnsRecord[] }) {
  if (records.length === 0) return <p className="text-sm text-zinc-500">No records yet.</p>;
  return (
    <>
      <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200 bg-white sm:hidden">
        {records.map((r, i) => (
          <li key={`${r.type}-${r.name}-${i}`} className="space-y-1.5 px-3 py-2.5">
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs">
                <span className="font-mono font-medium text-zinc-800">{r.type}</span>
                <span className="text-zinc-500"> · {purposeLabels[r.purpose] ?? r.purpose}</span>
                {r.priority !== undefined ? <span className="text-zinc-500"> · priority {r.priority}</span> : null}
              </span>
              <RecordStatus r={r} />
            </div>
            <div className="grid grid-cols-[3rem_minmax(0,1fr)] items-start gap-x-2 gap-y-1 text-[11px] text-zinc-500">
              <span className="pt-px">Name</span>
              <CopyCell value={r.name} label="name" />
              <span className="pt-px">Value</span>
              <div className="min-w-0">
                <CopyCell value={r.value} label="value" />
                <FoundNote r={r} />
              </div>
            </div>
          </li>
        ))}
      </ul>
      <div className="hidden sm:block">
        <RecordsTable records={records} />
      </div>
    </>
  );
}

function RecordsTable({ records }: { records: DnsRecord[] }) {
  const hasPriority = records.some((r) => r.priority !== undefined);
  return (
    <Table className="bg-white shadow-none">
      <thead>
        <tr>
          <Th>Type</Th>
          <Th>Name</Th>
          <Th>Value</Th>
          {hasPriority ? <Th>Priority</Th> : null}
          <Th>Status</Th>
        </tr>
      </thead>
      <tbody>
        {records.map((r, i) => (
          <tr key={`${r.type}-${r.name}-${i}`} className="align-top">
            <Td className="whitespace-nowrap">
              <div className="font-mono text-xs font-medium text-zinc-800">{r.type}</div>
              <div className="text-[11px] text-zinc-500">{purposeLabels[r.purpose] ?? r.purpose}</div>
            </Td>
            <Td className="max-w-[200px]">
              <CopyCell value={r.name} label="name" />
            </Td>
            <Td className="max-w-[280px]">
              <CopyCell value={r.value} label="value" />
              <FoundNote r={r} />
            </Td>
            {hasPriority ? <Td className="tabular-nums text-zinc-700">{r.priority ?? "—"}</Td> : null}
            <Td>
              <RecordStatus r={r} />
            </Td>
          </tr>
        ))}
      </tbody>
    </Table>
  );
}

function CopyCell({ value, label }: { value: string; label: string }) {
  return (
    <button
      type="button"
      onClick={() => void copyText(value, `Copied ${label}`)}
      className="group flex w-full items-center gap-1.5 text-left"
      title={`Copy ${label}`}
    >
      <code className="min-w-0 flex-1 truncate font-mono text-xs text-zinc-800">{value}</code>
      <Copy className="size-3 shrink-0 text-zinc-300 group-hover:text-zinc-600" />
    </button>
  );
}

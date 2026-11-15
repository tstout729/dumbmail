import { useState, type FormEvent, type ReactNode } from "react";
import type { UseMutationResult } from "@tanstack/react-query";
import { CheckCircle2, Copy, ExternalLink, RefreshCw, Trash2 } from "lucide-react";
import type { DnsPurpose, DnsRecord, Sending } from "../../api";
import {
  useRemoveDomain,
  useRemoveLinkDomain,
  useSending,
  useSession,
  useSetDomain,
  useSetLinkDomain,
  useVerifyDomain,
  useVerifyLinkDomain,
} from "../../api";
import { Badge, Button, Card, ConfirmDialog, Input, Notice, Skeleton, StatusBadge, Table, Td, Th } from "../../components/ui";
import { copyText } from "../../lib/clipboard";
import { toast } from "../../lib/toast";
import { hostnameOf } from "./shared";

const purposeLabels: Record<DnsPurpose, string> = {
  dkim: "DKIM signature",
  spf: "Return path (SPF)",
  mail_from: "Return path (MX)",
  dmarc: "DMARC policy",
  link: "Branded links",
  verification: "Domain verification",
};

export function DomainsPage() {
  const { account } = useSession();
  const sending = useSending();
  const setDomain = useSetDomain();
  const verifyDomain = useVerifyDomain();
  const removeDomain = useRemoveDomain();
  const setLink = useSetLinkDomain();
  const verifyLink = useVerifyLinkDomain();
  const removeLink = useRemoveLinkDomain();

  if (sending.isPending) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-56" />
        <Skeleton className="h-40" />
      </div>
    );
  }
  if (sending.error) return <p className="text-sm text-red-600">{sending.error.message}</p>;

  const { email, link_domain: link } = sending.data;
  const rootDomain = email.domain ?? account.from_email?.split("@")[1] ?? hostnameOf(account.website);

  return (
    <div className="space-y-4">
      <DomainSection
        title="Sending domain"
        intro={
          <>
            Gmail and Yahoo now require bulk senders to sign their mail with DKIM, line up SPF with their own domain and publish a DMARC
            policy. Mail without them goes to spam or gets rejected. These records prove your emails really come from you, which is the
            biggest single factor in reaching the inbox.
          </>
        }
        placeholder="yourdomain.com"
        suggested={rootDomain}
        domain={email.domain}
        status={email.status}
        ready={email.status === "verified"}
        readyText="Your domain is verified. Emails are signed and aligned for inbox placement."
        details={
          <>
            Provider: {email.provider}
            {email.mail_from_domain ? ` · Return path: ${email.mail_from_domain}` : ""}
          </>
        }
        records={email.dns_records}
        setMutation={setDomain}
        verifyMutation={verifyDomain}
        removeMutation={removeDomain}
        onVerified={(r) =>
          r.email.status === "verified"
            ? toast.success("Domain verified")
            : toast.info(notReadyMessage(r.email.dns_records))
        }
        zoneOf={(d) => d}
        removeBody="DumbMail stops signing your emails with this domain. You can add it again later, but you'll need to re-check the DNS records."
      />
      <DomainSection
        title="Branded link domain"
        intro={
          <>
            Makes tracking links, unsubscribe pages and sign-up forms use your domain, e.g. <code className="rounded bg-zinc-100 px-1 text-xs">links.yourbrand.com</code>.
            Links on your own domain look more trustworthy to subscribers and to spam filters. Optional.
          </>
        }
        placeholder="links.yourbrand.com"
        suggested={rootDomain ? `links.${rootDomain}` : ""}
        domain={link.domain}
        status={link.status}
        ready={link.status === "active"}
        readyText={
          link.url ? (
            <>
              Active. Links now use{" "}
              <a href={link.url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-0.5 font-medium underline">
                {link.url} <ExternalLink className="size-3" />
              </a>
            </>
          ) : (
            "Active. Your links now use this domain."
          )
        }
        records={link.dns_records}
        setMutation={setLink}
        verifyMutation={verifyLink}
        removeMutation={removeLink}
        onVerified={(r) =>
          r.link_domain.status === "active" ? toast.success("Link domain is active") : toast.info(notReadyMessage(r.link_domain.dns_records))
        }
        zoneOf={(d) => d.split(".").slice(1).join(".")}
        removeBody="New links go back to the default DumbMail domain. Links in emails you already sent keep working only while the DNS record stays in place."
      />
    </div>
  );
}

function notReadyMessage(records: DnsRecord[]): string {
  const missing = records.filter((r) => r.status === "missing" || r.status === "pending").length;
  return missing
    ? `${missing} ${missing === 1 ? "record isn't" : "records aren't"} showing up yet. DNS changes can take up to an hour.`
    : "Not verified yet. DNS changes can take up to an hour.";
}

type DomainSectionProps = {
  title: string;
  intro: ReactNode;
  placeholder: string;
  suggested: string;
  domain: string | null;
  status: string;
  ready: boolean;
  readyText: ReactNode;
  details?: ReactNode;
  records: DnsRecord[];
  setMutation: UseMutationResult<Sending, Error, string>;
  verifyMutation: UseMutationResult<Sending, Error, void>;
  removeMutation: UseMutationResult<Sending, Error, void>;
  onVerified: (result: Sending) => void;
  removeBody: string;
  /** The DNS zone the records live in, for the "enter only the host part" hint. */
  zoneOf: (domain: string) => string;
};

function DomainSection(props: DomainSectionProps) {
  const { title, intro, placeholder, suggested, domain, status, ready, readyText, details, records } = props;
  const { setMutation, verifyMutation, removeMutation, zoneOf } = props;
  const [changing, setChanging] = useState(false);
  const [confirmRemove, setConfirmRemove] = useState(false);

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const value = String(new FormData(e.currentTarget).get("domain")).trim().toLowerCase().replace(/^https?:\/\//, "").replace(/\/.*$/, "");
    setMutation.mutate(value, { onSuccess: () => setChanging(false) });
  };

  return (
    <Card title={title} actions={domain && status !== "not_started" ? <StatusBadge status={status} /> : null}>
      <p className="mb-4 text-sm text-zinc-600">{intro}</p>
      {!domain || changing ? (
        <form onSubmit={onSubmit} className="flex flex-wrap gap-2">
          <Input name="domain" required defaultValue={domain ?? suggested} placeholder={placeholder} className="max-w-xs" aria-label={`${title} name`} />
          <Button type="submit" variant="primary" loading={setMutation.isPending}>
            Get DNS records
          </Button>
          {changing ? (
            <Button variant="ghost" onClick={() => setChanging(false)}>
              Cancel
            </Button>
          ) : null}
        </form>
      ) : (
        <div className="space-y-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <div className="text-sm font-semibold text-zinc-900">{domain}</div>
              {details ? <div className="text-xs text-zinc-500">{details}</div> : null}
            </div>
            <div className="flex flex-wrap gap-2">
              <Button variant="ghost" size="sm" onClick={() => setChanging(true)}>
                Change
              </Button>
              <Button variant="ghost" size="sm" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirmRemove(true)}>
                Remove domain
              </Button>
              <Button
                size="sm"
                icon={<RefreshCw className="size-3.5" />}
                loading={verifyMutation.isPending}
                onClick={() => verifyMutation.mutate(undefined, { onSuccess: props.onVerified })}
              >
                Check DNS
              </Button>
            </div>
          </div>
          {ready ? (
            <Notice tone="green" icon={<CheckCircle2 />} title={readyText} />
          ) : (
            <p className="text-xs text-zinc-500">
              Add these records at your DNS host (Cloudflare, GoDaddy, Namecheap, Squarespace…), then check DNS. Changes can take up to an
              hour. If your host adds the domain for you, enter only the part of the name before <code>.{zoneOf(domain)}</code>.
            </p>
          )}
          <DnsRecordsTable records={records} />
        </div>
      )}
      {confirmRemove ? (
        <ConfirmDialog
          title={`Remove ${domain}?`}
          body={props.removeBody}
          confirmLabel="Remove domain"
          danger
          loading={removeMutation.isPending}
          onConfirm={() => removeMutation.mutate(undefined, { onSuccess: () => setConfirmRemove(false) })}
          onClose={() => setConfirmRemove(false)}
        />
      ) : null}
    </Card>
  );
}

function DnsRecordsTable({ records }: { records: DnsRecord[] }) {
  const hasPriority = records.some((r) => r.priority !== undefined);
  if (records.length === 0) return <p className="text-sm text-zinc-500">No records yet.</p>;
  return (
    <Table className="shadow-none">
      <thead>
        <tr>
          <Th>Record</Th>
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
              <Badge>{r.type}</Badge>
              <div className="mt-1 text-xs text-zinc-600">{purposeLabels[r.purpose] ?? r.purpose}</div>
            </Td>
            <Td className="max-w-[180px]">
              <CopyCell value={r.name} label="name" />
            </Td>
            <Td className="max-w-[260px]">
              <CopyCell value={r.value} label="value" />
              {r.status !== "verified" && r.found !== undefined ? (
                <div className="mt-1 text-[11px] text-zinc-500">
                  {r.found ? (
                    <>
                      DNS shows: <code className="break-all font-mono text-amber-700">{r.found}</code>
                    </>
                  ) : (
                    "Nothing found in public DNS yet"
                  )}
                </div>
              ) : null}
            </Td>
            {hasPriority ? <Td className="tabular-nums text-zinc-700">{r.priority ?? "—"}</Td> : null}
            <Td>
              <StatusBadge status={r.status} label={r.status === "recommended" ? "Optional" : undefined} />
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

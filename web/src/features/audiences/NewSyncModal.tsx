import { useState, type FormEvent } from "react";
import { useLists, useSegments } from "../../api";
import { errorMessage } from "../../api/client";
import { Button, Checkbox, Field, InlineError, Input, Modal, Notice, Select, Skeleton } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useAdAccounts, useCreateSync } from "./api";
import { intervalOptions } from "./SyncDrawer";
import type { AdConnection, AdPlatform, AdSettings } from "./types";

const OTHER = "__other__";

type Props = {
  connections: AdConnection[];
  settings: AdSettings | null;
  /** "list:<id>" or "segment:<id>" from a list or segment page's "Sync to ads". */
  initialSource: string | null;
  onClose: () => void;
  onCreated: (id: string) => void;
};

export function NewSyncModal({ connections, settings, initialSource, onClose, onCreated }: Props) {
  const usable = connections.filter((c) => c.status === "connected");
  const [chosenPlatform, setPlatform] = useState<AdPlatform | null>(null);
  // Connections may still be loading when this opens from a "Sync to ads" link.
  const platform = chosenPlatform ?? usable[0]?.platform ?? null;
  const [account, setAccount] = useState("");
  const [otherAccount, setOtherAccount] = useState("");
  const [manager, setManager] = useState("");
  const [source, setSource] = useState(initialSource ?? "");
  const [name, setName] = useState("");
  const [intervalHours, setIntervalHours] = useState(6);
  const [terms, setTerms] = useState(false);
  const lists = useLists();
  const segments = useSegments();
  const accounts = useAdAccounts(platform);
  const create = useCreateSync();

  const label = usable.find((c) => c.platform === platform)?.label ?? "the platform";
  const accountList = accounts.data?.data ?? [];
  const manual = accounts.data?.manual_entry ?? false;
  const typing = manual && (accountList.length === 0 || account === OTHER);
  const needsConsent = platform === "google" && !settings?.google_consent;
  const chosenAccount = typing ? otherAccount.trim() : account;
  const [sourceType, sourceId] = source.split(":") as ["list" | "segment" | "", string | undefined];
  const sourceName =
    sourceType === "list" ? lists.data?.find((l) => l.id === sourceId)?.name : sourceType === "segment" ? segments.data?.find((s) => s.id === sourceId)?.name : undefined;
  const ready = platform !== null && chosenAccount !== "" && sourceType !== "" && sourceId !== undefined && terms && !needsConsent;

  const choosePlatform = (p: AdPlatform) => {
    setPlatform(p);
    setAccount("");
    setOtherAccount("");
    setManager("");
  };

  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!ready) return;
    create.mutate(
      {
        platform,
        ad_account_id: chosenAccount,
        login_account_id: platform === "google" && manager.trim() ? manager.trim() : undefined,
        source_type: sourceType,
        source_id: sourceId,
        name: name.trim() || undefined,
        interval_hours: intervalHours,
        accept_terms: terms,
      },
      {
        onSuccess: (s) => {
          toast.success(`Syncing "${s.name}" to ${s.platform_label}`);
          onCreated(s.id);
        },
      },
    );
  };

  return (
    <Modal
      title="Sync to an ad audience"
      description="DumbMail creates the audience and keeps it in step as people join and leave."
      onClose={onClose}
      size="md"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" form="new-ad-sync" variant="primary" loading={create.isPending} disabled={!ready}>
            Create and sync
          </Button>
        </>
      }
    >
      <form id="new-ad-sync" onSubmit={onSubmit} className="space-y-4">
        <Field label="Platform">
          <Select value={platform ?? ""} onChange={(e) => choosePlatform(e.target.value as AdPlatform)}>
            {usable.map((c) => (
              <option key={c.platform} value={c.platform}>
                {c.label}
              </option>
            ))}
          </Select>
        </Field>

        {needsConsent ? (
          <Notice tone="amber" title="Turn on the Google consent setting first">
            Google syncs need the account's owner or an admin to confirm, under Google consent on this page, that your marketing consent covers advertising use.
          </Notice>
        ) : null}

        {manual && accountList.length === 0 && !accounts.error ? null : (
          <Field label="Ad account" error={accounts.error ? errorMessage(accounts.error) : undefined}>
            {accounts.isPending ? (
              <Skeleton className="h-8.5 w-full" />
            ) : accountList.length > 0 ? (
              <Select value={account} onChange={(e) => setAccount(e.target.value)} required>
                <option value="" disabled>
                  Choose an ad account
                </option>
                {accountList.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name ? `${a.name} (${a.id})` : a.id}
                  </option>
                ))}
                {manual ? <option value={OTHER}>Another account (enter its ID)</option> : null}
              </Select>
            ) : !manual && !accounts.error ? (
              <p className="text-sm text-zinc-500">This sign-in can't reach any {label} ad accounts. Check its permissions on {label}, then reconnect.</p>
            ) : null}
          </Field>
        )}
        {typing ? (
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Google Ads customer ID" hint="10 digits, like 123-456-7890. DumbMail checks this sign-in can manage its audiences.">
              <Input value={otherAccount} onChange={(e) => setOtherAccount(e.target.value)} inputMode="numeric" placeholder="123-456-7890" required />
            </Field>
            <Field label="Manager account ID" hint="Only if you reach it through a manager account.">
              <Input value={manager} onChange={(e) => setManager(e.target.value)} inputMode="numeric" placeholder="Optional" />
            </Field>
          </div>
        ) : null}

        <Field label="List or segment">
          <Select value={source} onChange={(e) => setSource(e.target.value)} required>
            <option value="" disabled>
              Choose a list or segment
            </option>
            <optgroup label="Lists">
              {(lists.data ?? []).map((l) => (
                <option key={l.id} value={`list:${l.id}`}>
                  {l.name} ({formatNumber(l.member_count)})
                </option>
              ))}
            </optgroup>
            <optgroup label="Segments">
              {(segments.data ?? []).map((s) => (
                <option key={s.id} value={`segment:${s.id}`}>
                  {s.name} ({formatNumber(s.member_count)})
                </option>
              ))}
            </optgroup>
          </Select>
        </Field>

        <Field label="Audience name" hint={`What it's called on ${label}.`}>
          <Input value={name} onChange={(e) => setName(e.target.value)} maxLength={100} placeholder={sourceName ? `${sourceName} (DumbMail)` : "Uses the list or segment name"} />
        </Field>

        <Field label="How often">
          <Select value={intervalHours} onChange={(e) => setIntervalHours(Number(e.target.value))}>
            {intervalOptions.map((o) => (
              <option key={o.hours} value={o.hours}>
                {o.label}
              </option>
            ))}
          </Select>
        </Field>

        <p className="text-xs text-zinc-500">
          Only people subscribed to your email who aren't suppressed, aren't on a marketing hold and haven't opted out of ads are sent: their hashed email, and their
          phone number if they opted in to texts. Nothing else, such as names or where people live, is sent.
          {platform === "tiktok" ? " TikTok audiences use email only, and TikTok needs at least 1,000 people before it makes one." : ""}
        </p>
        <Checkbox
          label={`These people gave us their contact details directly, and I accept ${label}'s customer list terms`}
          description="Contact details are hashed before they leave DumbMail."
          checked={terms}
          onChange={(e) => setTerms(e.target.checked)}
        />
        <InlineError message={create.error ? errorMessage(create.error) : null} />
      </form>
    </Modal>
  );
}

import { useState, type FormEvent } from "react";
import { useNavigate, useSearchParams } from "react-router";
import { Search, Upload, UserPlus } from "lucide-react";
import type { Consent } from "../../api";
import { useLists, useProfiles, useSegments, useUpsertProfile } from "../../api";
import { ImportDialog } from "../../components/audience/ImportDialog";
import { ProfilesTable } from "../../components/audience/ProfilesTable";
import { Button, Field, Input, Modal, Page, PageHeader, Select } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { BulkActionsBar } from "../../features/people/BulkActions";

const limit = 50;

export function ProfilesPage() {
  const [params, setParams] = useSearchParams();
  const [search, setSearch] = useState("");
  const q = useDebouncedValue(search.trim(), 250);
  // Paging belongs to one query; a new search starts at the first page.
  const listId = params.get("list_id") || undefined;
  const segmentId = params.get("segment_id") || undefined;
  const lists = useLists();
  const segments = useSegments();
  const filterKey = `${q}|${listId ?? ""}|${segmentId ?? ""}`;
  const [paging, setPaging] = useState({ q: "", offset: 0 });
  const offset = paging.q === filterKey ? paging.offset : 0;
  const profiles = useProfiles({ q: q || undefined, list_id: listId, segment_id: segmentId, limit, offset });
  // Picked rows belong to one search and filter; changing them starts a new selection.
  const [selection, setSelection] = useState<{ key: string; ids: Set<string> }>({ key: "", ids: new Set() });
  const selected = selection.key === filterKey ? selection.ids : new Set<string>();
  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelection({ key: filterKey, ids: next });
  };
  const pageIds = (profiles.data?.data ?? []).map((p) => p.id);
  const dialog = params.get("import") === "1" ? "import" : params.get("add") === "1" ? "add" : null;

  const setParam = (updates: Record<string, string | null>) => {
    const next = new URLSearchParams(params);
    for (const [k, v] of Object.entries(updates)) {
      if (v === null || v === "") next.delete(k);
      else next.set(k, v);
    }
    setParams(next, { replace: true });
  };

  return (
    <Page>
      <PageHeader
        title="Profiles"
        description={profiles.data ? `${formatNumber(profiles.data.total)} people` : "Everyone you can reach."}
        actions={
          <>
            <Button icon={<Upload className="size-3.5" />} onClick={() => setParam({ import: "1" })}>
              Import
            </Button>
            <Button variant="primary" icon={<UserPlus className="size-3.5" />} onClick={() => setParam({ add: "1" })}>
              Add profile
            </Button>
          </>
        }
      />
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <div className="relative w-full max-w-sm">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search by email, phone, name or ID"
            className="pl-8"
            autoFocus
          />
        </div>
        <Select value={listId ?? ""} onChange={(e) => setParam({ list_id: e.target.value, segment_id: null })} className="w-full sm:w-44" aria-label="List">
          <option value="">Any list</option>
          {(lists.data ?? []).map((l) => (
            <option key={l.id} value={l.id}>
              {l.name}
            </option>
          ))}
        </Select>
        <Select value={segmentId ?? ""} onChange={(e) => setParam({ segment_id: e.target.value, list_id: null })} className="w-full sm:w-44" aria-label="Segment">
          <option value="">Any segment</option>
          {(segments.data ?? []).map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}
            </option>
          ))}
        </Select>
      </div>
      <BulkActionsBar
        selected={selected}
        onClearSelection={() => setSelection({ key: filterKey, ids: new Set() })}
        filters={{ q: q || undefined, list_id: listId, segment_id: segmentId }}
        total={profiles.data?.total ?? 0}
        pageIds={pageIds}
        onSelectPage={() => setSelection({ key: filterKey, ids: new Set([...selected, ...pageIds]) })}
      />
      <ProfilesTable
        profiles={profiles.data?.data}
        total={profiles.data?.total ?? 0}
        loading={profiles.isPending}
        offset={offset}
        limit={limit}
        onPage={(o) => setPaging({ q: filterKey, offset: o })}
        selected={selected}
        onToggle={toggle}
        empty={
          q || listId || segmentId
            ? { title: "No matches", description: q ? `Nobody matches “${q}”.` : "Nobody in this list or segment yet." }
            : {
                title: "No profiles yet",
                description: "Import a CSV, add a sign-up form, or send events from your site.",
                action: (
                  <Button variant="primary" onClick={() => setParam({ import: "1" })}>
                    Import contacts
                  </Button>
                ),
              }
        }
      />
      {dialog === "import" ? <ImportDialog onClose={() => setParam({ import: null })} /> : null}
      {dialog === "add" ? <AddProfileDialog onClose={() => setParam({ add: null })} /> : null}
    </Page>
  );
}

function AddProfileDialog({ onClose }: { onClose: () => void }) {
  const upsert = useUpsertProfile();
  const lists = useLists();
  const navigate = useNavigate();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const str = (k: string) => String(d.get(k) ?? "").trim() || null;
    const listId = str("list_id");
    upsert.mutate(
      {
        email: str("email"),
        phone_number: str("phone_number"),
        first_name: str("first_name"),
        last_name: str("last_name"),
        email_consent: d.get("email_consent") as Consent,
        sms_consent: d.get("sms_consent") as Consent,
        list_ids: listId ? [listId] : undefined,
      },
      { onSuccess: (p) => navigate(`/audience/profiles/${p.id}`) },
    );
  };
  return (
    <Modal title="Add profile" onClose={onClose} size="md">
      <form onSubmit={onSubmit} className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Email" className="sm:col-span-2">
          <Input name="email" type="email" autoFocus />
        </Field>
        <Field label="Phone" hint="E.164, e.g. +15035550100" className="sm:col-span-2">
          <Input name="phone_number" type="tel" />
        </Field>
        <Field label="First name">
          <Input name="first_name" />
        </Field>
        <Field label="Last name">
          <Input name="last_name" />
        </Field>
        <Field label="Email consent">
          <Select name="email_consent" defaultValue="never_subscribed">
            <option value="subscribed">Subscribed</option>
            <option value="never_subscribed">Never subscribed</option>
            <option value="unsubscribed">Unsubscribed</option>
          </Select>
        </Field>
        <Field label="SMS consent">
          <Select name="sms_consent" defaultValue="never_subscribed">
            <option value="subscribed">Subscribed</option>
            <option value="never_subscribed">Never subscribed</option>
            <option value="unsubscribed">Unsubscribed</option>
          </Select>
        </Field>
        <Field label="Add to list" className="sm:col-span-2">
          <Select name="list_id" defaultValue="">
            <option value="">No list</option>
            {(lists.data ?? []).map((l) => (
              <option key={l.id} value={l.id}>
                {l.name}
              </option>
            ))}
          </Select>
        </Field>
        <div className="flex justify-end gap-2 sm:col-span-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={upsert.isPending}>
            Add profile
          </Button>
        </div>
      </form>
    </Modal>
  );
}

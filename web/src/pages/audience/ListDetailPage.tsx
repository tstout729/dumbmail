import { InlineTitle } from "../../components/ui/InlineTitle";
import { useState, type FormEvent } from "react";
import { useNavigate, useParams } from "react-router";
import { Megaphone, MoreHorizontal, Search, Settings2, Trash2, Upload, UserPlus } from "lucide-react";
import { type List, useAddListMembers, useDeleteList, useList, useProfiles, useRemoveListMembers, useUpdateList } from "../../api";
import { ImportDialog } from "../../components/audience/ImportDialog";
import { ProfilesTable } from "../../components/audience/ProfilesTable";
import { Badge, Button, Checkbox, ConfirmDialog, Field, IconButton, Input, Menu, Modal, Page, PageHeader, Skeleton, Textarea } from "../../components/ui";
import { useDebouncedValue } from "../../lib/hooks";
import { pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../../features/team/api";

const limit = 50;

export function ListDetailPage() {
  const { id = "" } = useParams();
  const list = useList(id);
  const [q, setQ] = useState("");
  const debouncedQ = useDebouncedValue(q, 250);
  const [offset, setOffset] = useState(0);
  const profiles = useProfiles({ list_id: id, q: debouncedQ || undefined, limit, offset });
  const [dialog, setDialog] = useState<"import" | "add" | "delete" | "settings" | null>(null);
  const canSyncAds = useCan("ad_audiences.manage");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [name, setName] = useState<string | null>(null);
  const rename = useUpdateList(id);
  const remove = useDeleteList();
  const removeMembers = useRemoveListMembers(id);
  const navigate = useNavigate();

  const toggle = (pid: string) => {
    const next = new Set(selected);
    if (next.has(pid)) next.delete(pid);
    else next.add(pid);
    setSelected(next);
  };

  if (list.error) {
    return (
      <Page>
        <PageHeader title="List" breadcrumbs={[{ label: "Lists & segments", to: "/audience/lists" }]} />
        <p className="text-sm text-red-600">{list.error.message}</p>
      </Page>
    );
  }

  return (
    <Page>
      <PageHeader
        breadcrumbs={[{ label: "Lists & segments", to: "/audience/lists" }]}
        title={
          list.data ? (
            <span className="flex items-center gap-2">
              <InlineTitle
                value={name ?? list.data.name}
                onChange={(e) => setName(e.target.value)}
                onBlur={() => {
                  if (name !== null && name.trim() && name !== list.data?.name) rename.mutate({ name: name.trim() }, { onSettled: () => setName(null) });
                  else setName(null);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") e.currentTarget.blur();
                }}
                aria-label="List name"
            />
              <Badge tone="blue">List</Badge>
              {list.data.double_opt_in ? <Badge>Double opt-in</Badge> : null}
              {list.data.show_on_preferences ? <Badge>On preference page</Badge> : null}
            </span>
          ) : (
            <Skeleton className="h-7 w-48" />
          )
        }
        description={list.data ? pluralize(list.data.member_count, "member") : undefined}
        actions={
          <>
            <Button icon={<Upload className="size-3.5" />} onClick={() => setDialog("import")}>
              Import
            </Button>
            <Button variant="primary" icon={<UserPlus className="size-3.5" />} onClick={() => setDialog("add")}>
              Add profiles
            </Button>
            <Menu
              trigger={(props) => (
                <IconButton label="More" {...props}>
                  <MoreHorizontal className="size-4" />
                </IconButton>
              )}
              items={[
                { label: "Settings", icon: <Settings2 className="size-3.5" />, onSelect: () => setDialog("settings") },
                ...(canSyncAds ? [{ label: "Sync to ads", icon: <Megaphone className="size-3.5" />, onSelect: () => navigate(`/settings/ad-audiences?new=list:${id}`) }] : []),
                { label: "Delete list", icon: <Trash2 className="size-3.5" />, danger: true, onSelect: () => setDialog("delete") },
              ]}
            />
          </>
        }
      />
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setOffset(0);
            }}
            placeholder="Search members"
            className="w-64 pl-8"
          />
        </div>
        {selected.size > 0 ? (
          <Button
            variant="danger"
            size="sm"
            loading={removeMembers.isPending}
            onClick={() =>
              removeMembers.mutate([...selected], {
                onSuccess: (r) => {
                  toast.success(`Removed ${pluralize(r.removed, "profile")}`);
                  setSelected(new Set());
                },
              })
            }
          >
            Remove {selected.size} from list
          </Button>
        ) : null}
      </div>
      <ProfilesTable
        profiles={profiles.data?.data}
        total={profiles.data?.total ?? 0}
        loading={profiles.isPending}
        offset={offset}
        limit={limit}
        onPage={setOffset}
        selected={selected}
        onToggle={toggle}
        empty={{ title: q ? "No matches" : "No members yet", description: "Import a CSV, add people by email, or connect a sign-up form." }}
      />
      {dialog === "import" ? <ImportDialog listId={id} onClose={() => setDialog(null)} /> : null}
      {dialog === "add" ? <AddMembersDialog listId={id} onClose={() => setDialog(null)} /> : null}
      {dialog === "settings" && list.data ? <ListSettingsDialog list={list.data} onClose={() => setDialog(null)} /> : null}
      {dialog === "delete" ? (
        <ConfirmDialog
          title="Delete this list?"
          body="Profiles stay in your account; only the list goes away. Flows and forms using it stop working."
          confirmLabel="Delete list"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(id, { onSuccess: () => navigate("/audience/lists") })}
          onClose={() => setDialog(null)}
        />
      ) : null}
    </Page>
  );
}

function ListSettingsDialog({ list, onClose }: { list: List; onClose: () => void }) {
  const update = useUpdateList(list.id);
  const [doubleOptIn, setDoubleOptIn] = useState(list.double_opt_in);
  const [onPreferences, setOnPreferences] = useState(list.show_on_preferences);
  const [description, setDescription] = useState(list.description);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    update.mutate(
      { double_opt_in: doubleOptIn, show_on_preferences: onPreferences, description: description.trim() },
      {
        onSuccess: () => {
          toast.success("List settings saved");
          onClose();
        },
      },
    );
  };
  return (
    <Modal title="List settings" onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <Checkbox
          checked={doubleOptIn}
          onChange={(e) => setDoubleOptIn(e.target.checked)}
          label="Double opt-in"
          description="People who sign up through a form confirm by email before they join."
        />
        <Checkbox
          checked={onPreferences}
          onChange={(e) => setOnPreferences(e.target.checked)}
          label="Show on the preference page"
          description="The unsubscribe link in your emails opens a page where people can join or leave this list themselves, instead of only unsubscribing from everything."
        />
        {onPreferences ? (
          <Field label="Description on the preference page" hint="Optional. Tell people what they'll get.">
            <Textarea value={description} onChange={(e) => setDescription(e.target.value)} rows={2} placeholder="New arrivals every Friday" />
          </Field>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={update.isPending}>
            Save
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function AddMembersDialog({ listId, onClose }: { listId: string; onClose: () => void }) {
  const add = useAddListMembers(listId);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const emails = String(new FormData(e.currentTarget).get("emails"))
      .split(/[\s,;]+/)
      .map((s) => s.trim())
      .filter((s) => s.includes("@"));
    add.mutate(
      { emails },
      {
        onSuccess: (r) => {
          toast.success(`Added ${pluralize(r.added, "profile")}`);
          onClose();
        },
      },
    );
  };
  return (
    <Modal title="Add profiles" onClose={onClose} size="md">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Email addresses" hint="One per line or comma-separated. New addresses create profiles.">
          <Textarea name="emails" rows={6} required autoFocus placeholder={"jane@example.com\nsam@example.com"} />
        </Field>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={add.isPending}>
            Add to list
          </Button>
        </div>
      </form>
    </Modal>
  );
}

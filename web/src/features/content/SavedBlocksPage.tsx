import { useState, type FormEvent } from "react";
import { Link, useBlocker, useNavigate, useParams } from "react-router";
import { Bookmark, Plus, Trash2 } from "lucide-react";
import type { EmailBlock } from "../../api/types";
import { BlockList } from "../../components/email/BlockList";
import { EmailPreview } from "../../components/email/EmailPreview";
import { newBlock } from "../../components/email/blocks";
import { Badge, Button, ConfirmDialog, EmptyState, Field, Input, Modal, Page, PageHeader, Skeleton, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { InlineTitle } from "../../components/ui/InlineTitle";
import { formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCreateSavedBlock, useDeleteSavedBlock, useSavedBlock, useSavedBlocks, useUpdateSavedBlock } from "./api";
import type { SavedBlockDetail } from "./types";

export function SavedBlocksPage() {
  const saved = useSavedBlocks();
  const [creating, setCreating] = useState(false);
  const rows = saved.data ?? [];
  return (
    <Page>
      <PageHeader
        title="Saved blocks"
        description="Headers, footers and other pieces you reuse. Edit one here and every email using it that hasn't been sent changes too."
        actions={
          <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => setCreating(true)}>
            New saved block
          </Button>
        }
      />
      {!saved.isPending && rows.length === 0 ? (
        <div className="rounded-lg border border-zinc-200 bg-white">
          <EmptyState
            icon={<Bookmark className="size-5" />}
            title="No saved blocks yet"
            description="Open any block in an email and choose “Save for reuse”, or create one here."
            action={
              <Button variant="primary" onClick={() => setCreating(true)}>
                New saved block
              </Button>
            }
          />
        </div>
      ) : (
        <Table>
          <thead>
            <tr>
              <Th>Name</Th>
              <Th className="hidden sm:table-cell">Blocks</Th>
              <Th>Used in</Th>
              <Th className="hidden md:table-cell">Edited</Th>
            </tr>
          </thead>
          <tbody>
            {saved.isPending ? (
              <SkeletonRows cols={4} />
            ) : (
              rows.map((s) => (
                <tr key={s.id} className="hover:bg-zinc-50">
                  <Td>
                    <Link to={`/content/saved-blocks/${s.id}`} className="font-medium text-zinc-900 hover:text-accent-800">
                      {s.name}
                    </Link>
                  </Td>
                  <Td className="hidden max-w-xs truncate text-zinc-500 sm:table-cell">{s.blocks.map((b) => b.type.replace("_", " ")).join(", ")}</Td>
                  <Td className="text-zinc-600">{s.uses ? `${s.uses} email${s.uses === 1 ? "" : "s"}` : "Not used yet"}</Td>
                  <Td className="hidden text-zinc-500 md:table-cell">{formatRelative(s.updated_at)}</Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      )}
      {creating ? <CreateDialog onClose={() => setCreating(false)} /> : null}
    </Page>
  );
}

function CreateDialog({ onClose }: { onClose: () => void }) {
  const create = useCreateSavedBlock();
  const navigate = useNavigate();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate({ name, blocks: [newBlock("footer")] }, { onSuccess: (s) => navigate(`/content/saved-blocks/${s.id}`) });
  };
  return (
    <Modal title="New saved block" onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Name">
          <Input name="name" required autoFocus maxLength={100} placeholder="Footer with social links" />
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

export function SavedBlockEditorPage() {
  const { id = "" } = useParams();
  const saved = useSavedBlock(id);
  if (saved.isPending) {
    return (
      <Page wide>
        <Skeleton className="mb-6 h-7 w-64" />
        <Skeleton className="h-[480px] w-full" />
      </Page>
    );
  }
  if (saved.error) {
    return (
      <Page>
        <PageHeader title="Saved block" breadcrumbs={[{ label: "Saved blocks", to: "/content/saved-blocks" }]} />
        <p className="text-sm text-red-600">{saved.error.message}</p>
      </Page>
    );
  }
  return <Editor key={saved.data.id} saved={saved.data} />;
}

type Draft = { name: string; blocks: EmailBlock[] };

function Editor({ saved }: { saved: SavedBlockDetail }) {
  const update = useUpdateSavedBlock(saved.id);
  const remove = useDeleteSavedBlock();
  const navigate = useNavigate();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const current = draft ?? { name: saved.name, blocks: saved.blocks };
  const dirty = draft !== null;
  const blocker = useBlocker(({ currentLocation, nextLocation }) => dirty && currentLocation.pathname !== nextLocation.pathname && !(nextLocation.state as { saved?: boolean } | null)?.saved);
  const live = saved.used_in.filter((u) => u.picks_up_changes);
  const kept = saved.used_in.filter((u) => !u.picks_up_changes);

  const save = () => {
    if (!draft) return;
    update.mutate(draft, {
      onSuccess: (r) => {
        setDraft(null);
        const n = r.used_in.filter((u) => u.picks_up_changes).length;
        toast.success(n ? `Saved. ${n} email${n === 1 ? "" : "s"} now show the change.` : "Saved");
      },
    });
  };

  const href = (u: SavedBlockDetail["used_in"][number]) => (u.kind === "template" ? `/content/templates/${u.id}` : u.kind === "campaign" ? `/campaigns/${u.id}` : `/flows/${u.id}`);

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Saved blocks", to: "/content/saved-blocks" }]}
        title={<InlineTitle value={current.name} onChange={(e) => setDraft({ ...current, name: e.target.value })} aria-label="Saved block name" />}
        actions={
          <>
            <Button variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirmDelete(true)}>
              Delete
            </Button>
            <Button variant={dirty ? "primary" : "secondary"} disabled={!dirty || current.blocks.length === 0} loading={update.isPending} onClick={save}>
              {dirty ? "Save" : "Saved"}
            </Button>
          </>
        }
      />
      <div className="mb-4 rounded-lg border border-zinc-200 bg-white px-4 py-3 text-sm">
        {saved.used_in.length === 0 ? (
          <span className="text-zinc-500">Not used in any email yet. Add it to an email from Add block → Saved block.</span>
        ) : (
          <div className="space-y-1">
            {live.length ? (
              <div className="flex flex-wrap items-center gap-1.5">
                <span className="text-zinc-600">Saving changes</span>
                {live.map((u) => (
                  <Link key={`${u.kind}-${u.id}`} to={href(u)} className="hover:underline">
                    <Badge>{u.name}</Badge>
                  </Link>
                ))}
              </div>
            ) : null}
            {kept.length ? (
              <div className="flex flex-wrap items-center gap-1.5 text-zinc-500">
                <span>Already sent, keeps what it sent:</span>
                {kept.map((u) => (
                  <Badge key={`${u.kind}-${u.id}`}>{u.name}</Badge>
                ))}
              </div>
            ) : null}
          </div>
        )}
      </div>
      <div className="grid min-h-0 grid-cols-1 gap-4 lg:grid-cols-[minmax(340px,440px)_minmax(0,1fr)]">
        <div className="min-w-0">
          <BlockList blocks={current.blocks} savedBlocks={false} onChange={(blocks) => setDraft({ ...current, blocks })} />
        </div>
        <EmailPreview content={{ subject: current.name, blocks: current.blocks }} className="min-h-[480px]" />
      </div>
      {confirmDelete ? (
        <ConfirmDialog
          title="Delete this saved block?"
          body={saved.used_in.length ? "It leaves this list. Emails that use it keep its current content." : "It leaves this list."}
          confirmLabel="Delete saved block"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(saved.id, { onSuccess: () => navigate("/content/saved-blocks", { state: { saved: true } }) })}
          onClose={() => setConfirmDelete(false)}
        />
      ) : null}
      {blocker.state === "blocked" ? (
        <ConfirmDialog title="Leave without saving?" body="Your changes to this saved block haven't been saved." confirmLabel="Discard changes" danger onConfirm={() => blocker.proceed()} onClose={() => blocker.reset()} />
      ) : null}
    </Page>
  );
}

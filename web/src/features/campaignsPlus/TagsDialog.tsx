import { useState } from "react";
import { Button, Field, InlineError, Modal, TagInput } from "../../components/ui";
import { useApplyTags, type ItemKind } from "./api";
import type { TagRef } from "./types";

/** Tag chips under an item's name. */
export function TagChips({ tags }: { tags: TagRef[] }) {
  if (tags.length === 0) return null;
  return (
    <div className="mt-1 flex flex-wrap gap-1">
      {tags.map((t) => (
        <span key={t.id} className="rounded bg-zinc-100 px-1.5 py-0.5 text-[11px] text-zinc-700">
          {t.name}
        </span>
      ))}
    </div>
  );
}

/** Edit one item's tags. */
export function TagsDialog({ kind, id, name, tags, onClose }: { kind: ItemKind; id: string; name: string; tags: TagRef[]; onClose: () => void }) {
  const apply = useApplyTags(kind);
  const before = tags.map((t) => t.name);
  const [value, setValue] = useState(before);
  const lower = (xs: string[]) => xs.map((x) => x.toLowerCase());
  const add = value.filter((v) => !lower(before).includes(v.toLowerCase()));
  const remove = before.filter((b) => !lower(value).includes(b.toLowerCase()));
  return (
    <Modal title={`Tags for "${name}"`} onClose={onClose} size="sm">
      <div className="space-y-4">
        <Field label="Tags" hint="Press Enter after each one. Up to 20, 40 characters each.">
          <TagInput value={value} onChange={setValue} placeholder="Promo" aria-label="Tags" />
        </Field>
        <InlineError message={apply.error?.message} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" loading={apply.isPending} disabled={add.length === 0 && remove.length === 0} onClick={() => apply.mutate({ ids: [id], add, remove }, { onSuccess: onClose })}>
            Save
          </Button>
        </div>
      </div>
    </Modal>
  );
}

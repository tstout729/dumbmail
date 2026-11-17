import { useState } from "react";
import { ArrowDown, ArrowUp, Bookmark, ChevronDown, Copy, GripVertical, Plus, Trash2 } from "lucide-react";
import type { EmailBlock, EmailBlockType } from "../../api";
import { IconButton, Menu, buttonClass, cx } from "../ui";
import { BlockFields } from "./BlockFields";
import { blockMeta, blockOrder, blockSummary, newBlock } from "./blocks";
import { BlockOptionsPanel, SavedBlockPicker } from "../../features/content/BlockExtras";

type BlockListProps = {
  blocks: EmailBlock[];
  onChange: (blocks: EmailBlock[]) => void;
  /** Nested lists (inside columns) can't contain more columns. */
  nested?: boolean;
  /** Offer saved blocks (off when editing a saved block itself). */
  savedBlocks?: boolean;
};

function AddBlockMenu({ onAdd, nested, label, onSaved }: { onAdd: (t: EmailBlockType) => void; nested?: boolean; label?: string; onSaved?: () => void }) {
  return (
    <Menu
      align="left"
      trigger={(props) => (
        <button type="button" {...props} className={buttonClass("ghost", "sm", "border border-dashed border-zinc-300 text-zinc-600")}>
          <Plus className="size-3.5" /> {label ?? "Add block"}
        </button>
      )}
      items={blockOrder
        // Columns and catalog product grids only go at the top level.
        .filter((t) => !nested || (t !== "columns" && t !== "products"))
        .map((t) => {
          const Icon = blockMeta[t].icon;
          return { label: blockMeta[t].label, icon: <Icon className="size-3.5 text-zinc-400" />, onSelect: () => onAdd(t) };
        })
        .concat(onSaved ? [{ label: "Saved block…", icon: <Bookmark className="size-3.5 text-zinc-400" />, onSelect: onSaved }] : [])}
    />
  );
}

export function BlockList({ blocks, onChange, nested, savedBlocks = true }: BlockListProps) {
  const [open, setOpen] = useState<number | null>(nested ? null : 0);
  const [dragFrom, setDragFrom] = useState<number | null>(null);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  /** Put `items` in place of block `i` (detaching a saved block), or insert them at `i` when `count` is 0. */
  const splice = (i: number, count: number, items: EmailBlock[]) => {
    const next = [...blocks];
    next.splice(i, count, ...items);
    onChange(next);
    setOpen(i);
  };

  const update = (i: number, b: EmailBlock) => onChange(blocks.map((x, j) => (j === i ? b : x)));
  const remove = (i: number) => {
    onChange(blocks.filter((_, j) => j !== i));
    setOpen(null);
  };
  const move = (from: number, to: number) => {
    if (to < 0 || to >= blocks.length || from === to) return;
    const next = [...blocks];
    const [item] = next.splice(from, 1);
    next.splice(to, 0, item);
    onChange(next);
    if (open === from) setOpen(to);
  };
  const duplicate = (i: number) => {
    const next = [...blocks];
    next.splice(i + 1, 0, structuredClone(blocks[i]));
    onChange(next);
    setOpen(i + 1);
  };
  const insert = (at: number, type: EmailBlockType) => {
    const next = [...blocks];
    next.splice(at, 0, newBlock(type));
    onChange(next);
    setOpen(at);
  };

  return (
    <div className="space-y-1.5">
      {blocks.map((block, i) => {
        const meta = blockMeta[block.type];
        const Icon = meta.icon;
        const isOpen = open === i;
        return (
          <div
            key={i}
            onDragOver={(e) => {
              if (dragFrom !== null) e.preventDefault();
            }}
            onDrop={(e) => {
              e.preventDefault();
              e.stopPropagation();
              if (dragFrom !== null) move(dragFrom, i);
              setDragFrom(null);
            }}
            className={cx(
              "group rounded-lg border bg-white transition-shadow",
              isOpen ? "border-accent-300 shadow-sm ring-1 ring-accent-100" : "border-zinc-200 hover:border-zinc-300",
              dragFrom === i && "opacity-50",
            )}
          >
            <div className="flex items-center gap-1 px-1.5 py-1.5">
              <span
                draggable
                onDragStart={(e) => {
                  e.stopPropagation();
                  setDragFrom(i);
                }}
                onDragEnd={() => setDragFrom(null)}
                className="cursor-grab p-1 text-zinc-300 hover:text-zinc-500 active:cursor-grabbing"
                aria-hidden
              >
                <GripVertical className="size-3.5" />
              </span>
              <button
                type="button"
                onClick={() => setOpen(isOpen ? null : i)}
                className="flex min-w-0 flex-1 items-center gap-2 text-left"
                aria-expanded={isOpen}
              >
                <Icon className="size-3.5 shrink-0 text-zinc-500" />
                <span className="text-xs font-medium text-zinc-800">{meta.label}</span>
                <span className="min-w-0 flex-1 truncate text-xs text-zinc-500">{blockSummary(block)}</span>
                <ChevronDown className={cx("size-3.5 shrink-0 text-zinc-400 transition-transform", isOpen && "rotate-180")} />
              </button>
              <div className="flex items-center opacity-60 group-hover:opacity-100">
                <IconButton size="sm" label="Move up" disabled={i === 0} onClick={() => move(i, i - 1)}>
                  <ArrowUp className="size-3" />
                </IconButton>
                <IconButton size="sm" label="Move down" disabled={i === blocks.length - 1} onClick={() => move(i, i + 1)}>
                  <ArrowDown className="size-3" />
                </IconButton>
                <IconButton size="sm" label="Duplicate" onClick={() => duplicate(i)}>
                  <Copy className="size-3" />
                </IconButton>
                <IconButton size="sm" label="Delete block" onClick={() => remove(i)} className="hover:text-red-600">
                  <Trash2 className="size-3" />
                </IconButton>
              </div>
            </div>
            {isOpen ? (
              <div className="space-y-3 border-t border-zinc-100 px-3 py-3">
                <BlockFields block={block} onChange={(b) => update(i, b)} />
                <BlockOptionsPanel block={block} nested={nested || !savedBlocks} onChange={(b) => update(i, b)} onReplace={(items) => splice(i, 1, items)} />
                {block.type === "columns" ? (
                  <div className={cx("grid gap-2", block.columns.length === 3 ? "lg:grid-cols-3" : "lg:grid-cols-2")}>
                    {block.columns.map((col, c) => (
                      <div key={c} className="rounded-md bg-zinc-50 p-2">
                        <div className="mb-1.5 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">Column {c + 1}</div>
                        <BlockList
                          nested
                          blocks={col}
                          onChange={(colBlocks) => update(i, { ...block, columns: block.columns.map((x, k) => (k === c ? colBlocks : x)) })}
                        />
                      </div>
                    ))}
                  </div>
                ) : null}
                {!nested ? (
                  <div className="flex justify-end">
                    <AddBlockMenu label="Insert below" onAdd={(t) => insert(i + 1, t)} onSaved={savedBlocks ? () => setSavedAt(i + 1) : undefined} />
                  </div>
                ) : null}
              </div>
            ) : null}
          </div>
        );
      })}
      <AddBlockMenu nested={nested} onAdd={(t) => insert(blocks.length, t)} onSaved={nested || !savedBlocks ? undefined : () => setSavedAt(blocks.length)} />
      {savedAt !== null ? <SavedBlockPicker onPick={(b) => splice(savedAt, 0, [b])} onClose={() => setSavedAt(null)} /> : null}
    </div>
  );
}

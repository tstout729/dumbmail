import { useState } from "react";
import { createPortal } from "react-dom";
import { Check, ListChecks, PieChart, Plus, X } from "lucide-react";
import type { Audience, AudienceRef } from "../../api";
import { useLists, useSegments } from "../../api";
import { formatNumber } from "../../lib/format";
import { useWindowKeydown } from "../../lib/hooks";
import { cx } from "../ui";

type Option = AudienceRef & { name: string; count: number };

function useAudienceOptions(): { options: Option[]; loading: boolean } {
  const lists = useLists();
  const segments = useSegments();
  const options: Option[] = [
    ...(lists.data ?? []).map((l) => ({ type: "list" as const, id: l.id, name: l.name, count: l.member_count })),
    ...(segments.data ?? []).map((s) => ({ type: "segment" as const, id: s.id, name: s.name, count: s.member_count })),
  ];
  return { options, loading: lists.isPending || segments.isPending };
}

const sameRef = (a: AudienceRef, b: AudienceRef) => a.type === b.type && a.id === b.id;

type PickerProps = { value: Audience; onChange: (a: Audience) => void; disabled?: boolean };

export function AudiencePicker({ value, onChange, disabled }: PickerProps) {
  const { options, loading } = useAudienceOptions();
  return (
    <div className="space-y-4">
      <RefMultiSelect
        label="Send to"
        hint="People in any of these lists or segments."
        value={value.include}
        options={options}
        loading={loading}
        disabled={disabled}
        exclude={value.exclude}
        onChange={(include) => onChange({ ...value, include })}
      />
      <RefMultiSelect
        label="Don't send to"
        hint="People in any of these are left out, even if they're included above."
        value={value.exclude}
        options={options}
        loading={loading}
        disabled={disabled}
        exclude={value.include}
        onChange={(exclude) => onChange({ ...value, exclude })}
      />
    </div>
  );
}

type MultiProps = {
  label: string;
  hint: string;
  value: AudienceRef[];
  options: Option[];
  exclude: AudienceRef[];
  loading: boolean;
  disabled?: boolean;
  onChange: (v: AudienceRef[]) => void;
};

function RefMultiSelect({ label, hint, value, options, exclude, loading, disabled, onChange }: MultiProps) {
  const [anchorRect, setAnchorRect] = useState<DOMRect | null>(null);
  const selected = value.map((ref) => options.find((o) => sameRef(o, ref)) ?? { ...ref, name: loading ? "…" : "Deleted", count: 0 });
  const toggle = (o: Option) =>
    onChange(value.some((v) => sameRef(v, o)) ? value.filter((v) => !sameRef(v, o)) : [...value, { type: o.type, id: o.id }]);

  return (
    <div>
      <div className="mb-1 text-xs font-medium text-zinc-700">{label}</div>
      <div className="flex flex-wrap items-center gap-1.5 rounded-md border border-zinc-300 bg-white p-1.5 shadow-xs">
        {selected.map((o) => (
          <span key={`${o.type}:${o.id}`} className="inline-flex items-center gap-1 rounded-md bg-zinc-100 py-0.5 pl-2 pr-1 text-xs text-zinc-800">
            {o.type === "list" ? <ListChecks className="size-3 text-zinc-500" /> : <PieChart className="size-3 text-zinc-500" />}
            {o.name}
            <span className="text-zinc-400">{formatNumber(o.count)}</span>
            {!disabled ? (
              <button type="button" aria-label={`Remove ${o.name}`} onClick={() => toggle(o)} className="rounded p-0.5 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-700">
                <X className="size-3" />
              </button>
            ) : null}
          </span>
        ))}
        {!disabled ? (
          <button
            type="button"
            onClick={(e) => setAnchorRect(e.currentTarget.getBoundingClientRect())}
            className="inline-flex items-center gap-1 rounded-md px-2 py-0.5 text-xs font-medium text-accent-700 hover:bg-accent-50"
          >
            <Plus className="size-3" /> Add list or segment
          </button>
        ) : null}
      </div>
      <p className="mt-1 text-xs text-zinc-500">{hint}</p>
      {anchorRect ? (
        <OptionPopover
          rect={anchorRect}
          options={options.filter((o) => !exclude.some((e) => sameRef(e, o)))}
          isSelected={(o) => value.some((v) => sameRef(v, o))}
          onToggle={toggle}
          onClose={() => setAnchorRect(null)}
        />
      ) : null}
    </div>
  );
}

type PopoverProps = {
  rect: DOMRect;
  options: Option[];
  isSelected: (o: Option) => boolean;
  onToggle: (o: Option) => void;
  onClose: () => void;
};

function OptionPopover({ rect, options, isSelected, onToggle, onClose }: PopoverProps) {
  const [q, setQ] = useState("");
  useWindowKeydown((e) => {
    if (e.key === "Escape") onClose();
  });
  const filtered = options.filter((o) => o.name.toLowerCase().includes(q.toLowerCase()));
  const top = Math.min(rect.bottom + 4, window.innerHeight - 320);
  const left = Math.min(rect.left, window.innerWidth - 300);
  return createPortal(
    <div className="fixed inset-0 z-50" onClick={onClose}>
      <div
        className="pl-pop fixed w-72 overflow-hidden rounded-lg border border-zinc-200 bg-white shadow-lg"
        style={{ top, left: Math.max(8, left) }}
        onClick={(e) => e.stopPropagation()}
      >
        <input
          autoFocus
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Search lists and segments"
          className="h-9 w-full border-b border-zinc-100 px-3 text-sm outline-none"
        />
        <ul className="max-h-64 overflow-y-auto p-1">
          {filtered.length === 0 ? <li className="px-3 py-4 text-center text-xs text-zinc-500">Nothing matches</li> : null}
          {filtered.map((o) => {
            const on = isSelected(o);
            return (
              <li key={`${o.type}:${o.id}`}>
                <button
                  type="button"
                  onClick={() => onToggle(o)}
                  className={cx("flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-sm hover:bg-zinc-100", on && "bg-accent-50")}
                >
                  {o.type === "list" ? <ListChecks className="size-3.5 text-zinc-400" /> : <PieChart className="size-3.5 text-zinc-400" />}
                  <span className="min-w-0 flex-1 truncate">{o.name}</span>
                  <span className="text-xs tabular-nums text-zinc-400">{formatNumber(o.count)}</span>
                  {on ? <Check className="size-3.5 text-accent-700" /> : <span className="size-3.5" />}
                </button>
              </li>
            );
          })}
        </ul>
      </div>
    </div>,
    document.body,
  );
}

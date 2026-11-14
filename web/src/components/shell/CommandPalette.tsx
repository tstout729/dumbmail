import { useState } from "react";
import { useNavigate } from "react-router";
import { createPortal } from "react-dom";
import { ArrowRight, CornerDownLeft, Plus, Search } from "lucide-react";
import { cx } from "../ui";
import { bottomNav, mainNav, settingsNav } from "./nav";
import { useCampaigns, useFlows, useSegments, useLists } from "../../api";

type Command = { id: string; label: string; hint?: string; group: string; to: string; icon?: "create" | "go" };

const createCommands: Command[] = [
  { id: "new-campaign", label: "Create campaign", group: "Create", to: "/campaigns?create=1", icon: "create" },
  { id: "new-flow", label: "Create flow", group: "Create", to: "/flows?create=1", icon: "create" },
  { id: "new-segment", label: "Create segment", group: "Create", to: "/audience/segments/new", icon: "create" },
  { id: "new-list", label: "Create list", group: "Create", to: "/audience/lists?create=list", icon: "create" },
  { id: "new-template", label: "Create template", group: "Create", to: "/content/templates?create=1", icon: "create" },
  { id: "new-form", label: "Create sign-up form", group: "Create", to: "/forms?create=1", icon: "create" },
  { id: "import", label: "Import contacts", group: "Create", to: "/audience/profiles?import=1", icon: "create" },
];

const pageCommands: Command[] = [
  ...mainNav.flatMap((g) =>
    g.items.map((i) => ({ id: `go-${i.to}`, label: i.label, hint: g.label, group: "Go to", to: i.to, icon: "go" as const })),
  ),
  ...bottomNav.map((i) => ({ id: `go-${i.to}`, label: i.label, group: "Go to", to: i.to, icon: "go" as const })),
  // Developers is already in bottomNav.
  ...settingsNav
    .filter((t) => t.to !== "/settings/developers")
    .map((t) => ({ id: `settings-${t.to}`, label: t.title, hint: "Settings", group: "Go to", to: t.to, icon: "go" as const })),
];

export function CommandPalette({ onClose }: { onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const navigate = useNavigate();
  const campaigns = useCampaigns();
  const flows = useFlows();
  const segments = useSegments();
  const lists = useLists();

  const records: Command[] = [
    ...(campaigns.data ?? []).map((c) => ({ id: c.id, label: c.name, hint: "Campaign", group: "Records", to: `/campaigns/${c.id}` })),
    ...(flows.data ?? []).map((f) => ({ id: f.id, label: f.name, hint: "Flow", group: "Records", to: `/flows/${f.id}` })),
    ...(segments.data ?? []).map((s) => ({ id: s.id, label: s.name, hint: "Segment", group: "Records", to: `/audience/segments/${s.id}` })),
    ...(lists.data ?? []).map((l) => ({ id: l.id, label: l.name, hint: "List", group: "Records", to: `/audience/lists/${l.id}` })),
  ];

  const q = query.trim().toLowerCase();
  const all = [...createCommands, ...pageCommands, ...(q ? records : [])];
  const results = q ? all.filter((c) => `${c.label} ${c.hint ?? ""}`.toLowerCase().includes(q)).slice(0, 30) : all;
  const clamped = Math.min(active, Math.max(results.length - 1, 0));

  const run = (cmd: Command | undefined) => {
    if (!cmd) return;
    onClose();
    navigate(cmd.to);
  };

  let lastGroup = "";
  return createPortal(
    <div className="fixed inset-0 z-[55] flex items-start justify-center bg-zinc-950/30 p-4 pt-[12vh]" onClick={onClose}>
      <div
        className="pl-pop w-full max-w-xl overflow-hidden rounded-xl bg-white shadow-2xl ring-1 ring-zinc-950/10"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-label="Command palette"
      >
        <div className="flex items-center gap-2 border-b border-zinc-100 px-4">
          <Search className="size-4 text-zinc-400" />
          <input
            autoFocus
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setActive(0);
            }}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setActive(Math.min(clamped + 1, results.length - 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setActive(Math.max(clamped - 1, 0));
              } else if (e.key === "Enter") {
                e.preventDefault();
                run(results[clamped]);
              } else if (e.key === "Escape") {
                onClose();
              }
            }}
            placeholder="Jump to a page, campaign, flow, segment… or create something"
            className="h-12 flex-1 bg-transparent text-sm outline-none placeholder:text-zinc-400"
          />
        </div>
        <ul className="max-h-[50vh] overflow-y-auto p-1.5" role="listbox">
          {results.length === 0 ? <li className="px-3 py-6 text-center text-sm text-zinc-500">No matches</li> : null}
          {results.map((cmd, i) => {
            const header = cmd.group !== lastGroup ? cmd.group : null;
            lastGroup = cmd.group;
            return (
              <li key={cmd.id + cmd.to}>
                {header ? <div className="px-2.5 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-zinc-400">{header}</div> : null}
                <button
                  type="button"
                  role="option"
                  aria-selected={i === clamped}
                  onMouseMove={() => setActive(i)}
                  onClick={() => run(cmd)}
                  className={cx(
                    "flex w-full items-center gap-2.5 rounded-md px-2.5 py-2 text-left text-sm",
                    i === clamped ? "bg-accent-50 text-accent-900" : "text-zinc-700",
                  )}
                >
                  {cmd.icon === "create" ? <Plus className="size-4 text-zinc-400" /> : <ArrowRight className="size-4 text-zinc-400" />}
                  <span className="flex-1 truncate">{cmd.label}</span>
                  {cmd.hint ? <span className="text-xs text-zinc-400">{cmd.hint}</span> : null}
                  {i === clamped ? <CornerDownLeft className="size-3.5 text-zinc-400" /> : null}
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

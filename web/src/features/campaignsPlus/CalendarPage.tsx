import { useState, type DragEvent } from "react";
import { Link, useSearchParams } from "react-router";
import { ChevronLeft, ChevronRight, Clock, List, Mail, MessageSquare } from "lucide-react";
import { useSession } from "../../api";
import { Badge, ButtonLink, ConfirmDialog, IconButton, InlineError, Page, PageHeader, Segmented, Skeleton, StatusBadge, cx } from "../../components/ui";
import { tzAbbrev, utcToZoned } from "../../lib/tz";
import { toast } from "../../lib/toast";
import { useCalendar, useReschedule } from "./api";
import type { CalendarItem } from "./types";

type View = "month" | "week";

/** "YYYY-MM-DD" day arithmetic, independent of the browser's time zone. */
function addDays(day: string, n: number): string {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d + n)).toISOString().slice(0, 10);
}
function weekday(day: string): number {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d)).getUTCDay();
}
function label(day: string, opts: Intl.DateTimeFormatOptions): string {
  const [y, m, d] = day.split("-").map(Number);
  return new Intl.DateTimeFormat("en-US", { ...opts, timeZone: "UTC" }).format(new Date(Date.UTC(y, m - 1, d)));
}

/** The days shown: a month as whole weeks (Sunday first), or one week. */
function range(view: View, anchor: string): { from: string; to: string; month: string } {
  if (view === "week") {
    const from = addDays(anchor, -weekday(anchor));
    return { from, to: addDays(from, 6), month: anchor.slice(0, 7) };
  }
  const first = `${anchor.slice(0, 7)}-01`;
  const from = addDays(first, -weekday(first));
  const lastOfMonth = addDays(`${addDays(first, 32).slice(0, 7)}-01`, -1);
  const to = addDays(lastOfMonth, 6 - weekday(lastOfMonth));
  return { from, to, month: anchor.slice(0, 7) };
}

const timingNote: Record<CalendarItem["timing"], string | null> = { all_at_once: null, local_time: "Local time", best_time: "Best time", gradual: "Gradual" };

export function CalendarPage() {
  const { account } = useSession();
  const tz = account.timezone;
  const today = utcToZoned(new Date().toISOString(), tz).date;
  const [params, setParams] = useSearchParams();
  const view: View = params.get("view") === "week" ? "week" : "month";
  const anchor = /^\d{4}-\d{2}-\d{2}$/.test(params.get("day") ?? "") ? (params.get("day") as string) : today;
  const { from, to, month } = range(view, anchor);
  const cal = useCalendar(from, to);
  const reschedule = useReschedule();
  const [moving, setMoving] = useState<{ item: CalendarItem; date: string } | null>(null);
  const [dragOver, setDragOver] = useState<string | null>(null);

  const go = (patch: { view?: View; day?: string }) => {
    const next = new URLSearchParams(params);
    if (patch.view) next.set("view", patch.view);
    if (patch.day) next.set("day", patch.day);
    setParams(next, { replace: true });
  };
  const step = (dir: 1 | -1) => {
    if (view === "week") return go({ day: addDays(anchor, 7 * dir) });
    const [y, m] = anchor.split("-").map(Number);
    go({ day: new Date(Date.UTC(y, m - 1 + dir, 1)).toISOString().slice(0, 10) });
  };

  const byDay = new Map<string, CalendarItem[]>();
  for (const it of cal.data?.items ?? []) byDay.set(it.date, [...(byDay.get(it.date) ?? []), it]);
  const days: string[] = [];
  for (let d = from; d <= to; d = addDays(d, 1)) days.push(d);

  const onDrop = (e: DragEvent, date: string) => {
    e.preventDefault();
    setDragOver(null);
    const id = e.dataTransfer.getData("text/campaign");
    const item = cal.data?.items.find((i) => i.id === id);
    if (item && item.date !== date) setMoving({ item, date });
  };

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Campaigns", to: "/campaigns" }]}
        title="Calendar"
        description={`Campaigns by send time in ${tz.replace(/_/g, " ")} (${tzAbbrev(tz)}). Drag a scheduled campaign to another day to move it; it keeps its time.`}
        actions={
          <ButtonLink to="/campaigns" variant="ghost" icon={<List className="size-3.5" />}>
            List
          </ButtonLink>
        }
      />
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-1">
          <IconButton label={view === "week" ? "Previous week" : "Previous month"} onClick={() => step(-1)}>
            <ChevronLeft className="size-4" />
          </IconButton>
          <IconButton label={view === "week" ? "Next week" : "Next month"} onClick={() => step(1)}>
            <ChevronRight className="size-4" />
          </IconButton>
          <button type="button" onClick={() => go({ day: today })} className="ml-1 rounded-md px-2 py-1 text-xs font-medium text-zinc-600 hover:bg-zinc-100">
            Today
          </button>
          <h2 className="ml-2 text-sm font-semibold text-zinc-900">
            {view === "week" ? `${label(from, { month: "short", day: "numeric" })} – ${label(to, { month: "short", day: "numeric", year: "numeric" })}` : label(`${month}-01`, { month: "long", year: "numeric" })}
          </h2>
        </div>
        <Segmented<View>
          value={view}
          onChange={(v) => go({ view: v })}
          options={[
            { value: "month", label: "Month" },
            { value: "week", label: "Week" },
          ]}
        />
      </div>
      <InlineError message={cal.error?.message} />
      <div className="overflow-hidden rounded-lg border border-zinc-200 bg-white">
        <div className="grid grid-cols-7 border-b border-zinc-200 bg-zinc-50 text-center text-[11px] font-medium uppercase tracking-wide text-zinc-500">
          {days.slice(0, 7).map((d) => (
            <div key={d} className="py-1.5">
              {label(d, { weekday: "short" })}
            </div>
          ))}
        </div>
        <div className="grid grid-cols-7">
          {days.map((d) => {
            const items = byDay.get(d) ?? [];
            const outside = view === "month" && d.slice(0, 7) !== month;
            return (
              <div
                key={d}
                onDragOver={(e) => {
                  if (e.dataTransfer.types.includes("text/campaign") && d >= today) {
                    e.preventDefault();
                    setDragOver(d);
                  }
                }}
                onDragLeave={() => setDragOver((cur) => (cur === d ? null : cur))}
                onDrop={(e) => onDrop(e, d)}
                className={cx(
                  "min-w-0 border-b border-r border-zinc-100 p-1 sm:p-1.5",
                  view === "week" ? "min-h-64" : "min-h-20 sm:min-h-28",
                  outside && "bg-zinc-50/60",
                  dragOver === d && "bg-accent-50 ring-2 ring-inset ring-accent-500",
                )}
              >
                <div className={cx("mb-1 text-right text-xs tabular-nums", d === today ? "font-semibold text-accent-700" : outside ? "text-zinc-400" : "text-zinc-600")}>
                  {d === today ? <span className="rounded-full bg-accent-600 px-1.5 py-0.5 text-white">{Number(d.slice(8))}</span> : Number(d.slice(8))}
                </div>
                {cal.isPending ? <Skeleton className="h-4 w-full" /> : null}
                <div className="space-y-1">
                  {items.map((it) => (
                    <CalendarEntry key={it.id} item={it} />
                  ))}
                </div>
              </div>
            );
          })}
        </div>
      </div>
      {cal.data && cal.data.items.length === 0 ? <p className="mt-3 text-sm text-zinc-500">Nothing scheduled or sent in these days.</p> : null}
      {cal.data?.truncated ? <p className="mt-3 text-xs text-zinc-500">Showing the first 1,000 campaigns in this range.</p> : null}
      {moving ? (
        <ConfirmDialog
          title={`Move "${moving.item.name}"?`}
          body={`It goes out ${label(moving.date, { weekday: "long", month: "short", day: "numeric" })} at ${moving.item.time} instead of ${label(moving.item.date, { month: "short", day: "numeric" })}. It's checked again as when you schedule it.`}
          confirmLabel="Move"
          loading={reschedule.isPending}
          onConfirm={() =>
            reschedule.mutate(
              { id: moving.item.id, date: moving.date },
              {
                onSuccess: () => {
                  toast.success("Moved");
                  setMoving(null);
                },
                onError: () => setMoving(null),
              },
            )
          }
          onClose={() => setMoving(null)}
        />
      ) : null}
    </Page>
  );
}

function CalendarEntry({ item }: { item: CalendarItem }) {
  const Icon = item.channel === "email" ? Mail : MessageSquare;
  const note = timingNote[item.timing];
  return (
    <Link
      to={`/campaigns/${item.id}`}
      draggable={item.can_move}
      onDragStart={(e) => {
        e.dataTransfer.setData("text/campaign", item.id);
        e.dataTransfer.effectAllowed = "move";
      }}
      title={[item.name, item.time, item.note].filter(Boolean).join(" · ")}
      className={cx(
        "block rounded border px-1 py-0.5 text-[11px] leading-tight transition-colors hover:border-zinc-300 sm:px-1.5 sm:text-xs",
        item.status === "scheduled" ? "border-accent-200 bg-accent-50/60" : item.status === "draft" ? "border-dashed border-zinc-300 bg-white" : "border-zinc-200 bg-zinc-50",
        item.can_move && "cursor-grab active:cursor-grabbing",
      )}
    >
      <span className="flex items-center gap-1">
        <Icon className="hidden size-3 shrink-0 text-zinc-400 sm:block" aria-label={item.channel === "email" ? "Email" : "SMS"} />
        <span className="hidden tabular-nums text-zinc-500 sm:inline">{item.time}</span>
        <span className="truncate font-medium text-zinc-900">{item.name}</span>
      </span>
      <span className="mt-0.5 hidden flex-wrap items-center gap-1 sm:flex">
        <StatusBadge status={item.status} />
        {note ? (
          <Badge tone="violet">
            <Clock className="mr-0.5 inline size-3" />
            {note}
          </Badge>
        ) : null}
        {item.resend ? <Badge tone="blue">Resend</Badge> : null}
      </span>
    </Link>
  );
}

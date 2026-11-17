import { useState } from "react";
import { useNavigate, useParams, useSearchParams } from "react-router";
import { CalendarClock, Copy, Download, MoreHorizontal, Plus, Save, Trash2, X } from "lucide-react";
import { useCampaigns, useFlows, useForms, useLists, useMetrics, useSegments, errorMessage } from "../../api";
import {
  Button,
  Card,
  Checkbox,
  ConfirmDialog,
  Field,
  IconButton,
  Input,
  Menu,
  Modal,
  Notice,
  Page,
  PageHeader,
  Segmented,
  Select,
  Skeleton,
  TagInput,
  Textarea,
  Toggle,
  cx,
} from "../../components/ui";
import { useDebouncedValue } from "../../lib/hooks";
import { formatDateTime } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useSession } from "../../api";
import { downloadCsv, useCatalog, useCreateReport, useDeleteReport, useDuplicateReport, useMembers, useRunReport, useSavedReport, useUpdateReport } from "./api";
import { ResultView } from "./ResultView";
import type { CatalogSource, ReportFilter, ReportSpec, SavedReport, Schedule } from "./types";

type RangeChoice = "7" | "30" | "90" | "365" | "custom";

const FILTER_LABELS: Record<string, string> = {
  channel: "Channel",
  campaign: "Campaign",
  flow: "Flow",
  list: "List",
  segment: "Segment",
  country: "Country",
  form: "Form",
  source: "Source",
  property: "Event property",
};

export function ReportPage() {
  const { id } = useParams();
  const report = useSavedReport(id);
  const catalog = useCatalog();
  const [params] = useSearchParams();
  if (catalog.error || report.error) {
    return (
      <Page>
        <PageHeader title="Report" breadcrumbs={[{ label: "Reports", to: "/analytics/reports" }]} />
        <p className="text-sm text-red-600">{errorMessage(catalog.error ?? report.error)}</p>
      </Page>
    );
  }
  if (!catalog.data || (id && !report.data)) {
    return (
      <Page>
        <Skeleton className="mb-4 h-8 w-64" />
        <Skeleton className="h-64 w-full" />
      </Page>
    );
  }
  const initial: ReportSpec = report.data?.spec ?? { source: params.get("source") ?? "campaigns", range: { days: 30 } };
  return <Builder key={id ?? "new"} sources={catalog.data.sources} report={report.data} initial={initial} />;
}

function rangeChoice(spec: ReportSpec): RangeChoice {
  if (spec.range?.start) return "custom";
  const d = String(spec.range?.days ?? 30);
  return d === "7" || d === "90" || d === "365" ? d : "30";
}

function Builder({ sources, report, initial }: { sources: CatalogSource[]; report: SavedReport | undefined; initial: ReportSpec }) {
  const navigate = useNavigate();
  const { account } = useSession();
  const [spec, setSpec] = useState<ReportSpec>(() => {
    const src = sources.find((s) => s.key === initial.source) ?? sources[0];
    return { ...initial, source: src.key, measures: initial.measures?.length ? initial.measures : src.default_measures };
  });
  const [saveOpen, setSaveOpen] = useState(false);
  const [scheduleOpen, setScheduleOpen] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [downloading, setDownloading] = useState(false);
  const src = sources.find((s) => s.key === spec.source) ?? sources[0];
  const debounced = useDebouncedValue(spec, 350);
  // Filters still being filled in don't count yet.
  const runnable: ReportSpec = { ...debounced, filters: (debounced.filters ?? []).filter((f) => f.values.length > 0 && (f.field !== "property" || Boolean(f.key))) };
  const ready = (debounced.source !== "metric" || Boolean(debounced.metric)) && (!(debounced.group_by ?? []).includes("property") || Boolean(debounced.property));
  const result = useRunReport(ready ? runnable : null);
  const update = useUpdateReport(report?.id ?? "");
  const remove = useDeleteReport();
  const duplicate = useDuplicateReport();
  const editable = report !== undefined && !report.builtin;
  const dirty = report !== undefined && JSON.stringify(report.spec) !== JSON.stringify(spec);

  const set = (patch: Partial<ReportSpec>) => setSpec((s) => ({ ...s, ...patch }));
  const pickSource = (key: string) => {
    const next = sources.find((s) => s.key === key) ?? sources[0];
    setSpec((s) => ({ source: next.key, measures: next.default_measures, range: s.range, compare: s.compare, group_by: [], filters: [] }));
  };
  const groups = spec.group_by ?? [];
  const setGroup = (i: number, v: string) => {
    const next = [...groups];
    if (v) next[i] = v;
    else next.splice(i);
    set({ group_by: next.filter(Boolean).slice(0, 2), sort: undefined });
  };
  const toggleMeasure = (key: string) => {
    const cur = spec.measures ?? [];
    const next = cur.includes(key) ? cur.filter((m) => m !== key) : [...cur, key];
    if (next.length) set({ measures: next, sort: spec.sort && !next.includes(spec.sort.by) ? undefined : spec.sort });
  };
  const choice = rangeChoice(spec);

  async function exportCsv() {
    setDownloading(true);
    try {
      await downloadCsv("/api/reports/run.csv", runnable, `${report?.name ?? "report"}.csv`);
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setDownloading(false);
    }
  }

  const title = report?.name ?? "New report";
  return (
    <Page wide>
      <PageHeader
        title={title}
        description={report?.description || src.description}
        breadcrumbs={[{ label: "Reports", to: "/analytics/reports" }]}
        actions={
          <>
            <Button icon={<Download className="size-4" />} loading={downloading} onClick={exportCsv}>
              Export CSV
            </Button>
            {editable ? (
              <>
                <Button icon={<CalendarClock className="size-4" />} onClick={() => setScheduleOpen(true)}>
                  {report.schedule ? `Emailed ${report.schedule}` : "Schedule email"}
                </Button>
                <Button
                  variant="primary"
                  icon={<Save className="size-4" />}
                  disabled={!dirty}
                  loading={update.isPending}
                  onClick={() => update.mutate({ spec: { ...spec, filters: (spec.filters ?? []).filter((f) => f.values.length > 0) } }, { onSuccess: () => toast.success("Report saved") })}
                >
                  Save
                </Button>
              </>
            ) : (
              <Button variant="primary" icon={<Save className="size-4" />} onClick={() => setSaveOpen(true)}>
                {report?.builtin ? "Save a copy" : "Save report"}
              </Button>
            )}
            {report ? (
              <Menu
                trigger={(p) => (
                  <IconButton label="More" {...p}>
                    <MoreHorizontal className="size-4" />
                  </IconButton>
                )}
                items={[
                  {
                    label: "Duplicate",
                    icon: <Copy className="size-4" />,
                    onSelect: () =>
                      duplicate.mutate(
                        { id: report.id },
                        {
                          onSuccess: (r) => {
                            toast.success("Report duplicated");
                            navigate(`/analytics/reports/${r.id}`);
                          },
                        },
                      ),
                  },
                  ...(editable ? [{ label: "Delete", icon: <Trash2 className="size-4" />, danger: true, onSelect: () => setDeleting(true) }] : []),
                ]}
              />
            ) : null}
          </>
        }
      />
      {report?.last_error ? (
        <div className="mb-4">
          <Notice tone="amber" title="The last scheduled email didn't go out">
            {report.last_error}
          </Notice>
        </div>
      ) : null}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-[300px_minmax(0,1fr)]">
        <Card className="h-fit lg:sticky lg:top-4">
          <div className="space-y-4">
            <Field label="Report on">
              <Select value={spec.source} onChange={(e) => pickSource(e.target.value)}>
                {sources.map((s) => (
                  <option key={s.key} value={s.key}>
                    {s.label}
                  </option>
                ))}
              </Select>
            </Field>
            {spec.source === "metric" ? <MetricPicker value={spec.metric ?? ""} onChange={(m) => set({ metric: m || undefined })} /> : null}
            {spec.source !== "engagement" ? (
              <Field label="Date range" hint={choice === "custom" ? `Dates in ${account.timezone}` : undefined}>
                <div className="space-y-2">
                  <Select
                    value={choice}
                    onChange={(e) => {
                      const v = e.target.value as RangeChoice;
                      if (v === "custom") {
                        const today = new Date().toISOString().slice(0, 10);
                        const start = new Date(Date.now() - 29 * 864e5).toISOString().slice(0, 10);
                        set({ range: { start, end: today } });
                      } else set({ range: { days: Number(v) } });
                    }}
                  >
                    <option value="7">Last 7 days</option>
                    <option value="30">Last 30 days</option>
                    <option value="90">Last 90 days</option>
                    <option value="365">Last 12 months</option>
                    <option value="custom">Custom dates</option>
                  </Select>
                  {choice === "custom" ? (
                    <div className="grid grid-cols-2 gap-2">
                      <Input type="date" aria-label="From" value={spec.range?.start ?? ""} onChange={(e) => set({ range: { ...spec.range, start: e.target.value } })} />
                      <Input type="date" aria-label="To" value={spec.range?.end ?? ""} onChange={(e) => set({ range: { ...spec.range, end: e.target.value } })} />
                    </div>
                  ) : null}
                  <label className="flex items-center justify-between gap-2 text-sm text-zinc-700">
                    Compare with the period before
                    <Toggle checked={Boolean(spec.compare)} onChange={(v) => set({ compare: v })} label="Compare with the period before" />
                  </label>
                </div>
              </Field>
            ) : null}
            {src.group_by.length > 1 || src.group_by[0]?.key !== "tier" ? (
              <Field label="Group by">
                <div className="space-y-2">
                  <Select value={groups[0] ?? ""} onChange={(e) => setGroup(0, e.target.value)}>
                    <option value="">Nothing (totals)</option>
                    {src.group_by.map((g) => (
                      <option key={g.key} value={g.key}>
                        {g.label}
                      </option>
                    ))}
                  </Select>
                  {groups[0] ? (
                    <Select value={groups[1] ?? ""} onChange={(e) => setGroup(1, e.target.value)} aria-label="Then by">
                      <option value="">Then by nothing</option>
                      {src.group_by
                        .filter((g) => g.key !== groups[0])
                        .map((g) => (
                          <option key={g.key} value={g.key}>
                            Then by {g.label.toLowerCase()}
                          </option>
                        ))}
                    </Select>
                  ) : null}
                  {groups.includes("property") ? (
                    <Input placeholder="Property name, e.g. category" value={spec.property ?? ""} onChange={(e) => set({ property: e.target.value || undefined })} aria-label="Property to group by" />
                  ) : null}
                </div>
              </Field>
            ) : null}
            <Field label="Measures">
              <div className="flex flex-wrap gap-1.5">
                {src.measures.map((m) => {
                  const on = spec.measures?.includes(m.key) ?? false;
                  return (
                    <button
                      key={m.key}
                      type="button"
                      aria-pressed={on}
                      onClick={() => toggleMeasure(m.key)}
                      className={cx(
                        "rounded-full border px-2.5 py-1 text-xs font-medium transition-colors",
                        on ? "border-accent-600 bg-accent-50 text-accent-800" : "border-zinc-200 bg-white text-zinc-600 hover:border-zinc-300",
                      )}
                    >
                      {m.label}
                    </button>
                  );
                })}
              </div>
            </Field>
            <Filters source={src} filters={spec.filters ?? []} onChange={(filters) => set({ filters })} />
          </div>
        </Card>
        <div className="min-w-0">
          {!ready ? (
            <Notice tone="gray" title={spec.source === "metric" && !spec.metric ? "Pick a metric to report on" : "Name the property to group by"} />
          ) : (
            <ResultView result={result.data} loading={result.isFetching} error={result.error ? errorMessage(result.error) : null} />
          )}
        </div>
      </div>
      {saveOpen ? <SaveDialog spec={spec} defaultName={report ? `${report.name} (copy)` : ""} onClose={() => setSaveOpen(false)} /> : null}
      {scheduleOpen && report ? <ScheduleDialog report={report} onClose={() => setScheduleOpen(false)} /> : null}
      {deleting && report ? (
        <ConfirmDialog
          title="Delete this report?"
          body={report.schedule ? "Its scheduled emails stop too. This can't be undone." : "This can't be undone."}
          confirmLabel="Delete"
          danger
          loading={remove.isPending}
          onClose={() => setDeleting(false)}
          onConfirm={() =>
            remove.mutate(report.id, {
              onSuccess: () => {
                toast.success("Report deleted");
                navigate("/analytics/reports");
              },
            })
          }
        />
      ) : null}
    </Page>
  );
}

function MetricPicker({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const metrics = useMetrics();
  return (
    <Field label="Metric">
      <Select value={value} onChange={(e) => onChange(e.target.value)}>
        <option value="">Pick a metric</option>
        {(metrics.data ?? []).map((m) => (
          <option key={m.id} value={m.name}>
            {m.name}
          </option>
        ))}
      </Select>
    </Field>
  );
}

function Filters({ source, filters, onChange }: { source: CatalogSource; filters: ReportFilter[]; onChange: (f: ReportFilter[]) => void }) {
  if (source.filters.length === 0) return null;
  const update = (i: number, f: ReportFilter) => onChange(filters.map((x, j) => (j === i ? f : x)));
  return (
    <Field label="Filters">
      <div className="space-y-2">
        {filters.map((f, i) => (
          <div key={i} className="space-y-1.5 rounded-md border border-zinc-200 p-2">
            <div className="flex items-center gap-1.5">
              <Select value={f.field} onChange={(e) => update(i, { field: e.target.value, values: [] })} aria-label="Filter field" className="flex-1">
                {source.filters.map((k) => (
                  <option key={k} value={k}>
                    {FILTER_LABELS[k] ?? k}
                  </option>
                ))}
              </Select>
              <Select value={f.exclude ? "not" : "is"} onChange={(e) => update(i, { ...f, exclude: e.target.value === "not" })} aria-label="Match" className="w-20">
                <option value="is">is</option>
                <option value="not">is not</option>
              </Select>
              <IconButton label="Remove filter" size="sm" onClick={() => onChange(filters.filter((_, j) => j !== i))}>
                <X className="size-3.5" />
              </IconButton>
            </div>
            <FilterValues filter={f} onChange={(next) => update(i, next)} />
          </div>
        ))}
        <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => onChange([...filters, { field: source.filters[0], values: [] }])}>
          Add filter
        </Button>
      </div>
    </Field>
  );
}

function OptionsSelect({ value, options, onChange, label }: { value: string; options: { id: string; name: string }[]; onChange: (v: string) => void; label: string }) {
  return (
    <Select value={value} onChange={(e) => onChange(e.target.value)} aria-label={label}>
      <option value="">Pick one</option>
      {options.map((o) => (
        <option key={o.id} value={o.id}>
          {o.name}
        </option>
      ))}
    </Select>
  );
}

function FilterValues({ filter, onChange }: { filter: ReportFilter; onChange: (f: ReportFilter) => void }) {
  const campaigns = useCampaigns();
  const flows = useFlows();
  const lists = useLists();
  const segments = useSegments();
  const forms = useForms();
  const one = (v: string) => onChange({ ...filter, values: v ? [v] : [] });
  const v = filter.values[0] ?? "";
  switch (filter.field) {
    case "channel":
      return <OptionsSelect label="Channel" value={v} onChange={one} options={[{ id: "email", name: "Email" }, { id: "sms", name: "SMS" }]} />;
    case "source":
      return <OptionsSelect label="Source" value={v} onChange={one} options={[{ id: "campaign", name: "Campaigns" }, { id: "flow", name: "Flows" }, { id: "none", name: "Not attributed" }]} />;
    case "campaign":
      return <OptionsSelect label="Campaign" value={v} onChange={one} options={campaigns.data ?? []} />;
    case "flow":
      return <OptionsSelect label="Flow" value={v} onChange={one} options={flows.data ?? []} />;
    case "list":
      return <OptionsSelect label="List" value={v} onChange={one} options={lists.data ?? []} />;
    case "segment":
      return <OptionsSelect label="Segment" value={v} onChange={one} options={segments.data ?? []} />;
    case "form":
      return <OptionsSelect label="Form" value={v} onChange={one} options={forms.data ?? []} />;
    case "property":
      return (
        <div className="space-y-1.5">
          <Input placeholder="Property name" value={filter.key ?? ""} onChange={(e) => onChange({ ...filter, key: e.target.value })} aria-label="Property name" />
          <TagInput value={filter.values} onChange={(values) => onChange({ ...filter, values })} placeholder="Values, then Enter" aria-label="Property values" />
        </div>
      );
    default:
      return <TagInput value={filter.values} onChange={(values) => onChange({ ...filter, values: values.map((x) => x.toUpperCase()) })} placeholder="Country codes, e.g. US" aria-label="Countries" />;
  }
}

function SaveDialog({ spec, defaultName, onClose }: { spec: ReportSpec; defaultName: string; onClose: () => void }) {
  const navigate = useNavigate();
  const create = useCreateReport();
  const [name, setName] = useState(defaultName);
  const [description, setDescription] = useState("");
  // Drop filters that were added but never filled in.
  const clean: ReportSpec = { ...spec, filters: (spec.filters ?? []).filter((f) => f.values.length > 0) };
  return (
    <Modal
      title="Save report"
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="primary"
            loading={create.isPending}
            disabled={!name.trim()}
            onClick={() =>
              create.mutate(
                { name, description, spec: clean },
                {
                  onSuccess: (r) => {
                    toast.success("Report saved");
                    onClose();
                    navigate(`/analytics/reports/${r.id}`);
                  },
                },
              )
            }
          >
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <Field label="Name">
          <Input value={name} onChange={(e) => setName(e.target.value)} maxLength={120} autoFocus />
        </Field>
        <Field label="Description" hint="Optional. Shown under the name.">
          <Textarea value={description} onChange={(e) => setDescription(e.target.value)} rows={2} maxLength={500} />
        </Field>
      </div>
    </Modal>
  );
}

function ScheduleDialog({ report, onClose }: { report: SavedReport; onClose: () => void }) {
  const { account } = useSession();
  const members = useMembers();
  const update = useUpdateReport(report.id);
  const [schedule, setSchedule] = useState<Schedule | "off">(report.schedule ?? "off");
  const [recipients, setRecipients] = useState<string[]>(report.recipients);
  const toggle = (id: string) => setRecipients((r) => (r.includes(id) ? r.filter((x) => x !== id) : [...r, id]));
  return (
    <Modal
      title="Email this report"
      description="A summary with the top rows, and a link to the full CSV (sign-in required, kept 7 days)."
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="primary"
            loading={update.isPending}
            disabled={schedule !== "off" && recipients.length === 0}
            onClick={() =>
              update.mutate(
                schedule === "off" ? { schedule: null } : { schedule, recipients },
                {
                  onSuccess: () => {
                    toast.success(schedule === "off" ? "Scheduled email turned off" : "Schedule saved");
                    onClose();
                  },
                },
              )
            }
          >
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Field label="How often" hint={`Sent at 8 a.m. ${account.timezone}.`}>
          <Segmented
            value={schedule}
            onChange={setSchedule}
            options={[
              { value: "off", label: "Off" },
              { value: "weekly", label: "Mondays" },
              { value: "monthly", label: "1st of the month" },
            ]}
          />
        </Field>
        {schedule !== "off" ? (
          <Field label="Send to">
            <div className="space-y-1.5">
              {(members.data ?? []).map((m) => (
                <Checkbox key={m.id} checked={recipients.includes(m.id)} onChange={() => toggle(m.id)} label={m.name || m.email} description={m.name ? m.email : undefined} />
              ))}
            </div>
          </Field>
        ) : null}
        {report.next_run_at && report.schedule ? <p className="text-xs text-zinc-500">Next email: {formatDateTime(report.next_run_at, account.timezone)}</p> : null}
      </div>
    </Modal>
  );
}

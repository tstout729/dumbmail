import { InlineTitle } from "../../components/ui/InlineTitle";
import { useState } from "react";
import { Link, useBlocker, useNavigate, useParams } from "react-router";
import { Loader2, Trash2, Users } from "lucide-react";
import type { Segment, SegmentDefinition } from "../../api";
import { useCreateSegment, useDeleteSegment, useSegment, useSegmentPreview, useUpdateSegment } from "../../api";
import { ConditionBuilder } from "../../components/segments/ConditionBuilder";
import { emptyDefinition, isDefinitionComplete } from "../../components/segments/conditions";
import { Badge, Button, Card, ConfirmDialog, Page, PageHeader, Skeleton, StatusBadge, cx } from "../../components/ui";
import { formatNumber, formatRelative, fullName } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { toast } from "../../lib/toast";

export function SegmentPage() {
  const { id = "" } = useParams();
  if (id === "new") return <SegmentEditor key="new" segment={null} />;
  return <ExistingSegment id={id} />;
}

function ExistingSegment({ id }: { id: string }) {
  const segment = useSegment(id);
  if (segment.isPending) {
    return (
      <Page>
        <Skeleton className="mb-6 h-7 w-64" />
        <Skeleton className="h-64 w-full" />
      </Page>
    );
  }
  if (segment.error) {
    return (
      <Page>
        <PageHeader title="Segment" breadcrumbs={[{ label: "Lists & segments", to: "/audience/lists" }]} />
        <p className="text-sm text-red-600">{segment.error.message}</p>
      </Page>
    );
  }
  return <SegmentEditor key={segment.data.id} segment={segment.data} />;
}

function SegmentEditor({ segment }: { segment: Segment | null }) {
  const navigate = useNavigate();
  const create = useCreateSegment();
  const update = useUpdateSegment(segment?.id ?? "");
  const remove = useDeleteSegment();
  const [name, setName] = useState(segment?.name ?? "");
  const [definition, setDefinition] = useState<SegmentDefinition>(segment?.definition ?? emptyDefinition());
  const [confirmDelete, setConfirmDelete] = useState(false);

  const dirty =
    segment === null
      ? definition.conditions.length > 0 || name !== ""
      : name !== segment.name || JSON.stringify(definition) !== JSON.stringify(segment.definition);
  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      dirty && currentLocation.pathname !== nextLocation.pathname && !(nextLocation.state as { saved?: boolean } | null)?.saved,
  );

  const debounced = useDebouncedValue(definition, 350);
  const complete = isDefinitionComplete(debounced) && debounced.conditions.length > 0;
  const preview = useSegmentPreview(complete ? debounced : null);
  const sample = complete ? (preview.data?.sample ?? []) : [];
  const sampleKeys = [...new Set(sample.flatMap((p) => Object.keys(p.properties)))];

  const canSave = name.trim() !== "" && isDefinitionComplete(definition) && definition.conditions.length > 0;
  const saving = create.isPending || update.isPending;

  const onSave = () => {
    if (segment === null) {
      create.mutate(
        { name: name.trim(), definition },
        {
          onSuccess: (s) => {
            toast.success("Segment created");
            navigate(`/audience/segments/${s.id}`, { replace: true, state: { saved: true } });
          },
        },
      );
    } else {
      update.mutate({ name: name.trim(), definition }, { onSuccess: () => toast.success("Segment saved") });
    }
  };

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Lists & segments", to: "/audience/lists" }]}
        title={
          <span className="flex items-center gap-2">
            <InlineTitle
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Name your segment"
              autoFocus={segment === null}
              aria-label="Segment name"
            />
            <Badge tone="violet">Segment</Badge>
          </span>
        }
        description={segment ? `${formatNumber(segment.member_count)} members${segment.refreshed_at ? ` · refreshed ${formatRelative(segment.refreshed_at)}` : ""}` : "Segments update automatically as people match or stop matching."}
        actions={
          <>
            {segment ? (
              <Button variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirmDelete(true)}>
                Delete
              </Button>
            ) : null}
            <Button variant="primary" disabled={!canSave || !dirty} loading={saving} onClick={onSave}>
              {segment ? (dirty ? "Save" : "Saved") : "Create segment"}
            </Button>
          </>
        }
      />
      <div className="grid gap-4 lg:grid-cols-[1fr_340px]">
        <Card title="Definition">
          <ConditionBuilder
            value={definition}
            onChange={setDefinition}
            propertySuggestions={sampleKeys}
            emptyHint="Add a condition, like “Placed Order at least once in the last 30 days” or “city equals Portland”."
          />
        </Card>
        <div className="space-y-4">
          <Card>
            <div className="flex items-center gap-2 text-xs font-medium text-zinc-500">
              <Users className="size-3.5" /> Matching profiles
              {preview.isFetching ? <Loader2 className="size-3 animate-spin" /> : null}
            </div>
            <div className={cx("mt-1 text-3xl font-semibold tabular-nums", preview.isFetching && "opacity-60")}>
              {!complete ? "—" : preview.data ? formatNumber(preview.data.count) : <Skeleton className="h-9 w-24" />}
            </div>
            <p className="mt-1 text-xs text-zinc-500">{complete ? "Live count, updates as you edit." : "Finish the conditions to see a live count."}</p>
            {preview.error ? <p className="mt-2 text-xs text-red-600">{preview.error.message}</p> : null}
          </Card>
          <Card title="Sample" padded={false}>
            {sample.length === 0 ? (
              <p className="px-4 py-6 text-center text-xs text-zinc-500">{complete ? "No one matches yet." : "A sample of matching people shows up here."}</p>
            ) : (
              <ul className="divide-y divide-zinc-100">
                {sample.slice(0, 10).map((p) => (
                  <li key={p.id}>
                    <Link to={`/audience/profiles/${p.id}`} className="flex items-center justify-between gap-2 px-4 py-2 hover:bg-zinc-50">
                      <span className="min-w-0">
                        <span className="block truncate text-sm text-zinc-900">{fullName(p) || p.email || p.phone_number || "Anonymous"}</span>
                        {fullName(p) && p.email ? <span className="block truncate text-xs text-zinc-500">{p.email}</span> : null}
                      </span>
                      <StatusBadge status={p.email_consent} />
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        </div>
      </div>
      {confirmDelete && segment ? (
        <ConfirmDialog
          title="Delete this segment?"
          body="Campaigns and flows that use it will stop matching anyone."
          confirmLabel="Delete segment"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(segment.id, {
              onSuccess: () => {
                navigate("/audience/lists?tab=segment", { state: { saved: true } });
              },
            })
          }
          onClose={() => setConfirmDelete(false)}
        />
      ) : null}
      {blocker.state === "blocked" ? (
        <ConfirmDialog
          title="Leave without saving?"
          body="Your changes to this segment haven't been saved."
          confirmLabel="Discard changes"
          danger
          onConfirm={() => blocker.proceed()}
          onClose={() => blocker.reset()}
        />
      ) : null}
    </Page>
  );
}

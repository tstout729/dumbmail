import { useState, type FormEvent } from "react";
import { useNavigate, useSearchParams } from "react-router";
import { ChevronDown, ListChecks, PieChart, Plus, Search } from "lucide-react";
import { useCreateList, useLists, useSegments, useSession } from "../../api";
import {
  Badge,
  Button,
  Checkbox,
  EmptyRow,
  EmptyState,
  Field,
  Input,
  Menu,
  Modal,
  Page,
  PageHeader,
  RowLink,
  SkeletonRows,
  Table,
  Tabs,
  Td,
  Th,
} from "../../components/ui";
import { formatDate, formatNumber, formatRelative } from "../../lib/format";

type Tab = "all" | "list" | "segment";

export function ListsPage() {
  const { account } = useSession();
  const lists = useLists();
  const segments = useSegments();
  const [params, setParams] = useSearchParams();
  const tab = (params.get("tab") as Tab | null) ?? "all";
  const creatingList = params.get("create") === "list";
  const [q, setQ] = useState("");
  const navigate = useNavigate();

  const rows = [
    ...(lists.data ?? []).map((l) => ({
      type: "list" as const,
      id: l.id,
      name: l.name,
      members: l.member_count,
      created: l.created_at,
      detail: l.double_opt_in ? "Double opt-in" : null,
      to: `/audience/lists/${l.id}`,
    })),
    ...(segments.data ?? []).map((s) => ({
      type: "segment" as const,
      id: s.id,
      name: s.name,
      members: s.member_count,
      created: s.created_at,
      detail: s.refreshed_at ? `Refreshed ${formatRelative(s.refreshed_at)}` : null,
      to: `/audience/segments/${s.id}`,
    })),
  ]
    .filter((r) => tab === "all" || r.type === tab)
    .filter((r) => r.name.toLowerCase().includes(q.toLowerCase()))
    .sort((a, b) => b.created.localeCompare(a.created));

  const setParam = (key: string, value: string | null) => {
    const next = new URLSearchParams(params);
    if (value === null) next.delete(key);
    else next.set(key, value);
    setParams(next, { replace: true });
  };
  const loading = lists.isPending || segments.isPending;

  return (
    <Page>
      <PageHeader
        title="Lists & segments"
        description="Lists are people you add. Segments update themselves from rules."
        actions={
          <Menu
            trigger={(props) => (
              <Button variant="primary" icon={<Plus className="size-4" />} {...props}>
                Create <ChevronDown className="size-3.5" />
              </Button>
            )}
            items={[
              { label: "List", icon: <ListChecks className="size-3.5" />, onSelect: () => setParam("create", "list") },
              { label: "Segment", icon: <PieChart className="size-3.5" />, onSelect: () => navigate("/audience/segments/new") },
            ]}
          />
        }
      />
      <div className="flex flex-wrap items-end justify-between gap-3">
        <Tabs
          value={tab}
          onChange={(t) => setParam("tab", t === "all" ? null : t)}
          tabs={[
            { value: "all", label: "All", count: lists.data && segments.data ? lists.data.length + segments.data.length : undefined },
            { value: "list", label: "Lists", count: lists.data?.length },
            { value: "segment", label: "Segments", count: segments.data?.length },
          ]}
        />
        <div className="relative mb-4">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search" className="w-56 pl-8" />
        </div>
      </div>
      <Table>
        <thead>
          <tr>
            <Th>Name</Th>
            <Th>Type</Th>
            <Th className="text-right">Members</Th>
            <Th>Created</Th>
          </tr>
        </thead>
        <tbody>
          {loading ? (
            <SkeletonRows cols={4} />
          ) : rows.length === 0 ? (
            <EmptyRow cols={4}>
              <EmptyState icon={<ListChecks className="size-5" />} title="Nothing here yet" description="Create a list to collect sign-ups, or a segment to target people by behavior." />
            </EmptyRow>
          ) : (
            rows.map((r) => (
              <RowLink key={`${r.type}:${r.id}`} to={r.to}>
                <Td>
                  <div className="flex items-center gap-2.5">
                    {r.type === "list" ? <ListChecks className="size-4 text-zinc-400" /> : <PieChart className="size-4 text-zinc-400" />}
                    <div>
                      <div className="font-medium text-zinc-900">{r.name}</div>
                      {r.detail ? <div className="text-xs text-zinc-500">{r.detail}</div> : null}
                    </div>
                  </div>
                </Td>
                <Td>
                  <Badge tone={r.type === "list" ? "blue" : "violet"}>{r.type === "list" ? "List" : "Segment"}</Badge>
                </Td>
                <Td className="text-right tabular-nums">{formatNumber(r.members)}</Td>
                <Td className="text-zinc-600">{formatDate(r.created, account.timezone)}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {creatingList ? <CreateListDialog onClose={() => setParam("create", null)} /> : null}
    </Page>
  );
}

function CreateListDialog({ onClose }: { onClose: () => void }) {
  const create = useCreateList();
  const navigate = useNavigate();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    create.mutate(
      { name: String(d.get("name")).trim(), double_opt_in: d.get("double_opt_in") === "on" },
      { onSuccess: (l) => navigate(`/audience/lists/${l.id}`) },
    );
  };
  return (
    <Modal title="Create list" onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="List name">
          <Input name="name" required autoFocus placeholder="Newsletter" />
        </Field>
        <Checkbox name="double_opt_in" label="Double opt-in" description="New sign-ups confirm by email before they're added." />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Create list
          </Button>
        </div>
      </form>
    </Modal>
  );
}

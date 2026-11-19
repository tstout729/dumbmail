import { useState, type FormEvent } from "react";
import { Download, FileSearch, Trash2 } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  EmptyRow,
  EmptyState,
  Field,
  InlineError,
  Input,
  Modal,
  Page,
  PageHeader,
  Segmented,
  SkeletonRows,
  Table,
  Td,
  Th,
} from "../../components/ui";
import { formatDateTime, formatNumber, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useDeletePerson, useExportPerson, usePrivacyRequests } from "./api";
import { ExportReady } from "./ProfilePanels";
import { useCan } from "../team/api";
import type { DeleteResult, ExportResult, PersonInput } from "./types";

export function PrivacyPage() {
  if (!useCan("privacy.manage")) {
    return (
      <Page>
        <PageHeader title="Privacy requests" description="When someone asks for a copy of their data, or asks you to delete it." />
        <EmptyState icon={<FileSearch className="size-5" />} title="Only admins and the owner can handle privacy requests" description="Ask an owner or admin to export or delete someone's data." />
      </Page>
    );
  }
  return <PrivacyRequests />;
}

function PrivacyRequests() {
  const { account } = useSession();
  const tz = account.timezone;
  const [by, setBy] = useState<"email" | "phone_number">("email");
  const [value, setValue] = useState("");
  const exportPerson = useExportPerson();
  const deletePerson = useDeletePerson();
  const [exported, setExported] = useState<ExportResult | null>(null);
  const [toDelete, setToDelete] = useState<{ input: PersonInput; dry: DeleteResult } | null>(null);
  const requests = usePrivacyRequests();
  const person: PersonInput = by === "email" ? { email: value.trim() } : { phone_number: value.trim() };

  const onExport = (e: FormEvent) => {
    e.preventDefault();
    if (!value.trim()) return;
    exportPerson.mutate(person, { onSuccess: setExported });
  };

  return (
    <Page>
      <PageHeader title="Privacy requests" description="When someone asks for a copy of their data, or asks you to delete it." />
      <Card title="Find the person">
        <form onSubmit={onExport} className="space-y-3">
          <Segmented
            value={by}
            onChange={(v) => {
              setBy(v);
              setValue("");
            }}
            options={[
              { value: "email", label: "Email" },
              { value: "phone_number", label: "Phone" },
            ]}
            size="sm"
          />
          <Field label={by === "email" ? "Their email address" : "Their phone number"}>
            <Input
              value={value}
              onChange={(e) => setValue(e.target.value)}
              type={by === "email" ? "email" : "tel"}
              placeholder={by === "email" ? "jane@example.com" : "+15035550100"}
              autoComplete="off"
            />
          </Field>
          <div className="flex flex-wrap gap-2">
            <Button type="submit" variant="primary" icon={<Download className="size-3.5" />} disabled={!value.trim()} loading={exportPerson.isPending}>
              Export their data
            </Button>
            <Button
              variant="danger"
              icon={<Trash2 className="size-3.5" />}
              disabled={!value.trim()}
              loading={deletePerson.isPending && !toDelete}
              onClick={() => deletePerson.mutate({ ...person, confirm: false }, { onSuccess: (dry) => setToDelete({ input: person, dry }) })}
            >
              Delete their data
            </Button>
          </div>
          <p className="text-xs text-zinc-500">
            An export is a JSON file with everything you hold about them and a readable page to send them. Downloads need you to be signed in and last 7 days. Deleting removes the profile and everything tied to it, and your stored export files (they may include them); if they had unsubscribed, a one-way code of the address is kept so they stay unsubscribed if they're imported again.
          </p>
          {exportPerson.error ? <InlineError message={errorMessage(exportPerson.error)} /> : null}
          {deletePerson.error && !toDelete ? <InlineError message={errorMessage(deletePerson.error)} /> : null}
        </form>
      </Card>

      <h2 className="mb-2 mt-6 text-sm font-semibold text-zinc-900">Request log</h2>
      <p className="mb-3 text-xs text-zinc-500">What was asked, when and by whom. The person's data isn't kept here.</p>
      <Table>
        <thead>
          <tr>
            <Th>When</Th>
            <Th>Request</Th>
            <Th>Found by</Th>
            <Th>By</Th>
            <Th>Result</Th>
          </tr>
        </thead>
        <tbody>
          {requests.isPending ? (
            <SkeletonRows cols={5} rows={4} />
          ) : (requests.data?.data ?? []).length === 0 ? (
            <EmptyRow cols={5}>
              <EmptyState icon={<FileSearch className="size-5" />} title="No requests yet" description="Exports and deletions you run appear here." />
            </EmptyRow>
          ) : (
            (requests.data?.data ?? []).map((r) => (
              <tr key={r.id}>
                <Td className="whitespace-nowrap text-zinc-600">{formatDateTime(r.created_at, tz)}</Td>
                <Td>
                  <Badge tone={r.kind === "delete" ? "red" : "blue"}>{r.kind === "delete" ? "Delete" : "Export"}</Badge>
                </Td>
                <Td className="text-zinc-600">{r.identified_by === "email" ? "Email" : "Phone"}</Td>
                <Td className="text-zinc-600">{r.requested_by ?? (r.via === "mcp" ? "AI assistant" : "API key")}</Td>
                <Td className="text-zinc-600">
                  {r.profiles_found === 0 ? (
                    "Nobody found"
                  ) : r.kind === "delete" ? (
                    `Deleted ${pluralize(Number(r.result.deleted_profiles ?? r.profiles_found), "profile")}`
                  ) : r.status === "building" ? (
                    "Building…"
                  ) : r.status === "failed" ? (
                    <span className="text-red-600">{typeof r.result.error === "string" ? r.result.error : "Didn't finish"}</span>
                  ) : r.files.length > 0 ? (
                    <span className="flex flex-wrap gap-x-3">
                      {r.files.map((f) => (
                        <a key={f.id} href={f.url} download className="text-accent-700 hover:underline">
                          {f.name.endsWith(".html") ? "HTML" : "JSON"}
                        </a>
                      ))}
                    </span>
                  ) : (
                    "Exported (download expired)"
                  )}
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
      {exported ? <ExportReady result={exported} tz={tz} onClose={() => setExported(null)} /> : null}
      {toDelete ? <ConfirmDelete pending={toDelete} onClose={() => setToDelete(null)} onDone={() => setValue("")} /> : null}
    </Page>
  );
}

function ConfirmDelete({ pending, onClose, onDone }: { pending: { input: PersonInput; dry: DeleteResult }; onClose: () => void; onDone: () => void }) {
  const remove = useDeletePerson();
  const [typed, setTyped] = useState("");
  const found = pending.dry.found ?? 0;
  const who = pending.input.email ?? pending.input.phone_number ?? "";
  if (found === 0) {
    return (
      <Modal title="Nobody to delete" onClose={onClose} size="sm" footer={<Button onClick={onClose}>OK</Button>}>
        <p className="text-sm text-zinc-600">Nobody with {who} is in this account, so you hold no data about them.</p>
      </Modal>
    );
  }
  const counts = (pending.dry.profiles ?? []).reduce((a, p) => ({ events: a.events + p.events, messages: a.messages + p.messages }), { events: 0, messages: 0 });
  return (
    <Modal
      title={`Delete everything about ${who}?`}
      onClose={onClose}
      size="sm"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="danger"
            disabled={typed.trim().toLowerCase() !== "delete"}
            loading={remove.isPending}
            onClick={() =>
              remove.mutate(
                { ...pending.input, confirm: true },
                {
                  onSuccess: () => {
                    toast.success("Their data is deleted");
                    onDone();
                    onClose();
                  },
                },
              )
            }
          >
            Delete permanently
          </Button>
        </>
      }
    >
      <div className="space-y-3 text-sm text-zinc-600">
        <p>
          This deletes {pluralize(found, "profile")} with {formatNumber(counts.events)} events, {formatNumber(counts.messages)} messages, their list memberships, notes and consent history, plus any earlier exports of their data. Your other stored export files (bulk and report exports) are deleted too, since they may include this person. It can't be undone.
        </p>
        <Field label="Type delete to confirm">
          <Input value={typed} onChange={(e) => setTyped(e.target.value)} autoFocus aria-label="Type delete to confirm" />
        </Field>
      </div>
    </Modal>
  );
}

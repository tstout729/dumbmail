import { useState } from "react";
import { FileUp } from "lucide-react";
import type { Consent, ImportResult } from "../../api";
import { useImportProfiles, useLists } from "../../api";
import { formatNumber } from "../../lib/format";
import { Button, Checkbox, Field, InlineError, Modal, Select, Textarea, cx } from "../ui";

type Props = { onClose: () => void; listId?: string };

export function ImportDialog({ onClose, listId }: Props) {
  const importer = useImportProfiles();
  const lists = useLists();
  const [csv, setCsv] = useState("");
  const [fileName, setFileName] = useState<string | null>(null);
  const [targetList, setTargetList] = useState(listId ?? "");
  const [emailConsent, setEmailConsent] = useState<Consent>("subscribed");
  const [smsConsent, setSmsConsent] = useState<Consent | "">("");
  const [result, setResult] = useState<ImportResult | null>(null);
  const [permission, setPermission] = useState(false);
  const needsPermission = emailConsent !== "unsubscribed";
  const [dragging, setDragging] = useState(false);

  const readFile = async (file: File | undefined) => {
    if (!file) return;
    setFileName(file.name);
    setCsv(await file.text());
  };

  const run = () =>
    importer.mutate(
      {
        csv,
        list_id: targetList || undefined,
        email_consent: emailConsent,
        sms_consent: smsConsent || undefined,
        permission_confirmed: needsPermission ? permission : undefined,
      },
      { onSuccess: setResult },
    );

  return (
    <Modal
      title="Import contacts"
      description="CSV with a header row. Known columns: email, phone_number, first_name, last_name, external_id. Other columns become custom properties."
      onClose={onClose}
      size="lg"
      footer={
        result ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!csv.trim() || (needsPermission && !permission)} loading={importer.isPending} onClick={run}>
              Import
            </Button>
          </>
        )
      }
    >
      {result ? (
        <div className="space-y-3">
          <div className="grid grid-cols-3 gap-3">
            {(
              [
                ["Created", result.created],
                ["Updated", result.updated],
                ["Skipped", result.skipped],
              ] as const
            ).map(([label, n]) => (
              <div key={label} className="rounded-lg border border-zinc-200 px-4 py-3">
                <div className="text-xs text-zinc-500">{label}</div>
                <div className="text-xl font-semibold tabular-nums">{formatNumber(n)}</div>
              </div>
            ))}
          </div>
          {result.errors.length > 0 ? (
            <div className="max-h-48 overflow-y-auto rounded-md bg-amber-50 p-3 text-xs text-amber-800">
              {result.errors.slice(0, 100).map((e, i) => (
                <div key={i}>{e}</div>
              ))}
            </div>
          ) : null}
        </div>
      ) : (
        <div className="space-y-3">
          <label
            onDragOver={(e) => {
              e.preventDefault();
              setDragging(true);
            }}
            onDragLeave={() => setDragging(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDragging(false);
              void readFile(e.dataTransfer.files[0]);
            }}
            className={cx(
              "flex cursor-pointer flex-col items-center gap-1 rounded-lg border-2 border-dashed px-4 py-5 text-center",
              dragging ? "border-accent-500 bg-accent-50" : "border-zinc-300 hover:border-zinc-400",
            )}
          >
            <FileUp className="size-5 text-zinc-400" />
            <span className="text-sm font-medium text-zinc-700">{fileName ?? "Drop a CSV or click to choose"}</span>
            <input type="file" accept=".csv,text/csv" className="sr-only" onChange={(e) => void readFile(e.target.files?.[0])} />
          </label>
          <Textarea rows={6} value={csv} onChange={(e) => setCsv(e.target.value)} placeholder={"email,first_name,city\njane@example.com,Jane,Portland"} className="font-mono text-xs" aria-label="CSV" />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <Field label="Add to list">
              <Select value={targetList} onChange={(e) => setTargetList(e.target.value)}>
                <option value="">No list</option>
                {(lists.data ?? []).map((l) => (
                  <option key={l.id} value={l.id}>
                    {l.name}
                  </option>
                ))}
              </Select>
            </Field>
            <Field label="Email consent">
              <Select value={emailConsent} onChange={(e) => setEmailConsent(e.target.value as Consent)}>
                <option value="subscribed">Subscribed</option>
                <option value="never_subscribed">Never subscribed</option>
                <option value="unsubscribed">Unsubscribed</option>
              </Select>
            </Field>
            <Field label="SMS consent">
              <Select value={smsConsent} onChange={(e) => setSmsConsent(e.target.value as Consent | "")}>
                <option value="">Leave as is</option>
                <option value="subscribed">Subscribed</option>
                <option value="never_subscribed">Never subscribed</option>
                <option value="unsubscribed">Unsubscribed</option>
              </Select>
            </Field>
          </div>
          {needsPermission ? (
            <Checkbox
              checked={permission}
              onChange={(e) => setPermission(e.target.checked)}
              label="Everyone in this file agreed to get email from me"
              description="For example, they signed up on your site or opted in at checkout. Bought, rented or scraped lists aren't allowed and get sending paused. Shared inboxes like info@ and throwaway addresses are skipped."
            />
          ) : (
            <p className="text-xs text-zinc-500">These people will be marked unsubscribed and won't get marketing email.</p>
          )}
          <InlineError message={importer.error?.message} />
        </div>
      )}
    </Modal>
  );
}

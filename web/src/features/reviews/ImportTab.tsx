import { useState, type ChangeEvent } from "react";
import { Download, Upload } from "lucide-react";
import { Badge, Button, Card, Checkbox, Field, InlineError, Notice, Select, Table, Td, Th } from "../../components/ui";
import { formatNumber, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { exportReviews, useImportReviews, useImportRun } from "./api";
import type { ImportField, ImportPreview, ImportRun } from "./types";

const fieldLabels: Record<ImportField, string> = {
  product_id: "Product id",
  product_handle: "Product handle or page",
  rating: "Rating (1-5)",
  title: "Title",
  body: "Review text",
  name: "Reviewer's name",
  email: "Reviewer's email",
  date: "Date",
  verified: "Verified buyer",
  photos: "Photo addresses",
  reply: "Your reply",
};

function isPreview(v: ImportPreview | ImportRun): v is ImportPreview {
  return "dry_run" in v;
}

export function ImportTab() {
  const canImport = useCan("catalog.delete");
  const canExport = useCan("export");
  return (
    <div className="space-y-4">
      {canImport ? <ImportCard /> : <Notice tone="gray" title="Importing reviews needs an Admin or Owner" />}
      <Card title="Export">
        <p className="text-sm text-zinc-600">Download every review as a CSV file: product, rating, text, name, date, status and, if you can see profiles, the reviewer's email.</p>
        <div className="mt-3">
          <Button icon={<Download className="size-3.5" />} disabled={!canExport} onClick={() => exportReviews({}).catch((e: Error) => toast.error(e.message))}>
            Export all reviews
          </Button>
        </div>
      </Card>
    </div>
  );
}

function ImportCard() {
  const importer = useImportReviews();
  const [csv, setCsv] = useState("");
  const [fileName, setFileName] = useState("");
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [mapping, setMapping] = useState<Partial<Record<ImportField, string>>>({});
  const [publish, setPublish] = useState(true);
  const [runId, setRunId] = useState<string | null>(null);
  const run = useImportRun(runId);

  const check = (text: string, map: Partial<Record<ImportField, string>>) =>
    importer.mutate(
      { csv: text, mapping: map, publish },
      {
        onSuccess: (v) => {
          if (isPreview(v)) {
            setPreview(v);
            setMapping(v.mapping);
          }
        },
      },
    );
  const onFile = (e: ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    if (!f) return;
    if (f.size > 10 * 1024 * 1024) {
      toast.error("The CSV can be at most 10 MB.");
      return;
    }
    setFileName(f.name);
    setRunId(null);
    void f.text().then((text) => {
      setCsv(text);
      check(text, {});
    });
  };
  const start = () =>
    importer.mutate(
      { csv, mapping, publish, confirm: true },
      {
        onSuccess: (v) => {
          if (!isPreview(v)) {
            setRunId(v.id);
            setPreview(null);
            toast.success("Import started");
          }
        },
      },
    );

  return (
    <Card title="Import reviews">
      <div className="space-y-4">
        <p className="text-sm text-zinc-600">Bring in reviews from another app's CSV export. You'll see how the columns match before anything is imported. Imported reviews keep their date, name and verified mark, and are labelled "Imported" here.</p>
        <label className="inline-flex cursor-pointer items-center gap-2 rounded-md border border-zinc-300 bg-white px-3 py-1.5 text-sm font-medium text-zinc-800 shadow-xs hover:bg-zinc-50">
          <Upload className="size-3.5" />
          {fileName ? `Change file (${fileName})` : "Choose a CSV file"}
          <input type="file" accept=".csv,text/csv" className="sr-only" onChange={onFile} />
        </label>
        <InlineError message={importer.error?.message} />
        {preview ? (
          <div className="space-y-4">
            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {preview.fields.map((f) => (
                <Field key={f} label={fieldLabels[f]}>
                  <Select
                    value={mapping[f] ?? ""}
                    onChange={(e) => {
                      const next = { ...mapping, [f]: e.target.value };
                      setMapping(next);
                      check(csv, next);
                    }}
                  >
                    <option value="">Not in the file</option>
                    {preview.headers.map((h) => (
                      <option key={h} value={h}>
                        {h}
                      </option>
                    ))}
                  </Select>
                </Field>
              ))}
            </div>
            {preview.ready ? (
              <>
                <Notice tone="blue" title={`${pluralize(preview.importable ?? 0, "review")} ready to import`}>
                  {preview.skipped ? `${pluralize(preview.skipped, "row")} will be skipped. ` : ""}
                  {preview.note}
                </Notice>
                {preview.sample?.length ? (
                  <Table>
                    <thead>
                      <tr>
                        <Th>Product</Th>
                        <Th>Rating</Th>
                        <Th>Review</Th>
                        <Th className="hidden sm:table-cell">Name</Th>
                      </tr>
                    </thead>
                    <tbody>
                      {preview.sample.map((s, i) => (
                        <tr key={i}>
                          <Td className="font-mono text-xs">{s.product}</Td>
                          <Td>{s.rating}</Td>
                          <Td className="max-w-xs truncate">{s.title || s.body}</Td>
                          <Td className="hidden sm:table-cell">
                            {s.name} {s.verified ? <Badge tone="green">Verified</Badge> : null}
                          </Td>
                        </tr>
                      ))}
                    </tbody>
                  </Table>
                ) : null}
                {preview.errors?.length ? (
                  <ul className="space-y-0.5 text-xs text-zinc-600">
                    {preview.errors.slice(0, 10).map((e) => (
                      <li key={e.row}>
                        Row {e.row}: {e.error}
                      </li>
                    ))}
                  </ul>
                ) : null}
                <Checkbox label="Publish them" description="They were public where they came from. Untick to check each one first." checked={publish} onChange={(e) => setPublish(e.target.checked)} />
                <Button variant="primary" loading={importer.isPending} onClick={start}>
                  Import {pluralize(preview.importable ?? 0, "review")}
                </Button>
              </>
            ) : (
              <Notice tone="amber" title="Choose the columns">
                {preview.note}
              </Notice>
            )}
          </div>
        ) : null}
        {run.data ? (
          <Notice tone={run.data.status === "failed" ? "red" : run.data.status === "done" ? "green" : "blue"} title={run.data.status === "done" ? "Import finished" : run.data.status === "failed" ? "Import failed" : "Importing…"}>
            {formatNumber(run.data.imported)} imported, {formatNumber(run.data.skipped)} skipped of {formatNumber(run.data.total)}.
            {run.data.errors.length ? ` ${run.data.errors.slice(0, 3).map((e) => (e.row ? `Row ${e.row}: ${e.error}` : e.error)).join(" · ")}` : ""}
          </Notice>
        ) : null}
      </div>
    </Card>
  );
}

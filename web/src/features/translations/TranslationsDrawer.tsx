import { useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import { Download, Eye, Sparkles, Upload } from "lucide-react";
import { errorMessage } from "../../api/client";
import { toast } from "../../lib/toast";
import { Badge, Button, ConfirmDialog, Drawer, EmptyState, InlineError, Input, Menu, Modal, Select, Skeleton, Textarea } from "../../components/ui";
import { downloadCsv } from "../reports/api";
import { contentApi, languageName, messageId, startDraft, storedTranslations, useLanguageSettings, useRun, type DraftQuote } from "./api";
import type { TranslationStatus, TranslationString, TranslationTarget, TranslationView } from "./types";

const statusLabel: Record<TranslationStatus, { label: string; tone: "gray" | "amber" | "green" | "red" | "blue" }> = {
  missing: { label: "Missing", tone: "red" },
  machine: { label: "Machine draft", tone: "amber" },
  check: { label: "Check numbers and links", tone: "red" },
  reviewed: { label: "Reviewed", tone: "green" },
  stale: { label: "Out of date", tone: "amber" },
  default: { label: "Same as default", tone: "gray" },
};

type Props<C> = {
  channel: "email" | "sms";
  value: C;
  onChange: (value: C) => void;
  /** The saved message, for Draft with AI (it works on the saved version). */
  target?: TranslationTarget;
  name?: string;
  readOnly?: boolean;
  onClose: () => void;
};

/** The language versions of the message being edited. Changes go into the editor's content and are saved with it. */
export function TranslationsDrawer<C>({ channel, value, onChange, target, name, readOnly, onClose }: Props<C>) {
  const settings = useLanguageSettings();
  const langs = settings.data?.languages ?? [];
  const [chosen, setChosen] = useState<string | null>(null);
  const lang = chosen ?? langs[0] ?? null;
  const view = useQuery({ queryKey: ["translations", "inspect", lang, value], queryFn: () => contentApi.inspect(value, lang), enabled: lang !== null });

  return (
    <Drawer title="Languages" onClose={onClose} width="w-[min(960px,100vw)]">
      {settings.isPending ? (
        <Skeleton className="h-40 w-full" />
      ) : langs.length === 0 ? (
        <EmptyState
          title="Send this in more than one language"
          description="Add the languages you send in first. Each person then gets the version in their language, and everyone else this one."
          action={
            <Link to="/settings/languages" className="text-sm font-medium text-accent-700 hover:underline">
              Settings → Languages
            </Link>
          }
        />
      ) : (
        <div className="space-y-4">
          <div className="flex flex-wrap items-center gap-2">
            <Select value={lang ?? ""} onChange={(e) => setChosen(e.target.value)} className="h-8 w-auto text-sm" aria-label="Language">
              {langs.map((l) => (
                <option key={l} value={l}>
                  {languageName(l)} ({l})
                </option>
              ))}
            </Select>
            {view.data && lang ? <Summary view={view.data} lang={lang} /> : null}
          </div>
          {view.isPending ? (
            <Skeleton className="h-64 w-full" />
          ) : view.error ? (
            <InlineError message={view.error.message} />
          ) : lang ? (
            <Editor
              key={lang}
              channel={channel}
              value={value}
              onChange={onChange}
              view={view.data}
              lang={lang}
              target={target}
              name={name}
              readOnly={readOnly}
              machine={settings.data?.machine_translation.available ?? false}
            />
          ) : null}
        </div>
      )}
    </Drawer>
  );
}

function Summary({ view, lang }: { view: TranslationView; lang: string }) {
  const s = view.summary[lang];
  if (!s) return <span className="text-xs text-zinc-500">Not translated yet. Texts without a translation go out in {languageName(view.default_language)}.</span>;
  return (
    <div className="flex flex-wrap gap-1">
      {s.missing > 0 ? <Badge tone="red">{s.missing} missing</Badge> : null}
      {s.stale > 0 ? <Badge tone="amber">{s.stale} out of date</Badge> : null}
      {s.check > 0 ? <Badge tone="red">{s.check} to check carefully</Badge> : null}
      {s.machine > 0 ? <Badge tone="amber">{s.machine} to review</Badge> : null}
      <Badge tone="green">{s.reviewed} reviewed</Badge>
    </div>
  );
}

type EditorProps<C> = {
  channel: "email" | "sms";
  value: C;
  onChange: (value: C) => void;
  view: TranslationView;
  lang: string;
  target?: TranslationTarget;
  name?: string;
  readOnly?: boolean;
  machine: boolean;
};

function Editor<C>({ channel, value, onChange, view, lang, target, name, readOnly, machine }: EditorProps<C>) {
  const [edits, setEdits] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [runId, setRunId] = useState<string | null>(null);
  const [preview, setPreview] = useState<{ subject?: string; html?: string; body?: string } | null>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const run = useRun(runId);
  const dirty = Object.keys(edits).length > 0;

  const act = async (f: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const applyEdits = () =>
    act(async () => {
      const strings = Object.entries(edits).map(([id, text]) => ({ id, text: text === "" ? null : text }));
      const r = await contentApi.apply(value, lang, strings);
      onChange(r.content);
      setEdits({});
      toast.success("Translations updated; save the message to keep them");
    });

  const markReviewed = (ids: string[]) =>
    act(async () => {
      const strings = view.strings.filter((s) => ids.includes(s.id)).map((s) => ({ id: s.id, text: s.text, status: "reviewed" as const }));
      const r = await contentApi.apply(value, lang, strings);
      onChange(r.content);
    });

  const importFile = (file: File) =>
    act(async () => {
      const data = await file.text();
      const format = file.name.toLowerCase().endsWith(".csv") ? "csv" : "xliff";
      const r = await contentApi.importFile(value, lang, format, data, messageId(target));
      onChange(r.content);
      const i = r.import;
      if (i) {
        const extra = [i.changed_since_export.length ? `${i.changed_since_export.length} changed since the export (skipped)` : "", i.refused.length ? `${i.refused.length} refused` : ""].filter(Boolean).join(", ");
        toast.success(`Imported ${i.applied} ${i.applied === 1 ? "translation" : "translations"}${extra ? `; ${extra}` : ""}`);
        if (i.refused.length) setError(i.refused.map((x) => `${x.id}: ${x.why}`).join("\n"));
      }
    });

  const [quote, setQuote] = useState<{ ids?: string[]; q: DraftQuote } | null>(null);
  const draft = (ids?: string[], confirm = false) =>
    act(async () => {
      if (!target) return;
      const r = await startDraft(target, lang, ids, confirm);
      // A run that costs money asks first.
      if ("dry_run" in r) setQuote({ ids, q: r });
      else setRunId(r.id);
    });

  // A finished run's drafts are added on request, into the content as it is now.
  const addDrafts = () =>
    act(async () => {
      if (!target) return;
      const stored = await storedTranslations(target, lang);
      const current = new Map(view.strings.map((s) => [s.id, s]));
      const drafts = stored.strings.filter((s) => s.status === "machine" && current.get(s.id)?.source === s.source && current.get(s.id)?.status !== "reviewed");
      setRunId(null);
      if (drafts.length === 0) {
        toast.success("No new drafts: the texts changed since the saved version");
        return;
      }
      const r = await contentApi.apply(value, lang, drafts.map((s) => ({ id: s.id, text: s.text, status: "machine" as const })));
      onChange(r.content);
      toast.success(`${drafts.length} machine ${drafts.length === 1 ? "draft" : "drafts"} added; review them before sending`);
    });

  const exportAs = (format: "xliff" | "csv") => act(() => downloadCsv("/api/translations/export", { content: value, language: lang, format, name: name ?? "message", message: messageId(target) }, `message.${lang}.${format === "csv" ? "csv" : "xlf"}`));
  const showPreview = () => act(async () => setPreview(await contentApi.preview(channel, value, lang)));
  const machineIds = view.strings.filter((s) => s.status === "machine" || s.status === "check").map((s) => s.id);
  const running = run.data && (run.data.status === "queued" || run.data.status === "running");

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="primary" disabled={!dirty || readOnly} loading={busy && dirty} onClick={applyEdits}>
          Use these translations
        </Button>
        {machineIds.length > 0 ? (
          <Button size="sm" disabled={readOnly || busy} onClick={() => markReviewed(machineIds)}>
            Mark all reviewed
          </Button>
        ) : null}
        {machine && target ? (
          <Button size="sm" icon={<Sparkles className="size-3.5" />} disabled={readOnly || busy || Boolean(running)} loading={Boolean(running)} onClick={() => draft()}>
            Draft with AI
          </Button>
        ) : null}
        <Menu
          align="left"
          trigger={(props) => (
            <Button size="sm" variant="ghost" icon={<Download className="size-3.5" />} {...props}>
              Export
            </Button>
          )}
          items={[
            { label: "XLIFF (for translation tools)", onSelect: () => exportAs("xliff") },
            { label: "CSV (for spreadsheets)", onSelect: () => exportAs("csv") },
          ]}
        />
        <Button size="sm" variant="ghost" icon={<Upload className="size-3.5" />} disabled={readOnly || busy} onClick={() => fileRef.current?.click()}>
          Import
        </Button>
        <input
          ref={fileRef}
          type="file"
          accept=".xlf,.xliff,.csv,.xml,text/csv,application/xml"
          className="hidden"
          onChange={(e) => {
            const f = e.target.files?.[0];
            e.target.value = "";
            if (f) importFile(f);
          }}
        />
        <Button size="sm" variant="ghost" icon={<Eye className="size-3.5" />} disabled={busy} onClick={showPreview}>
          Preview in {languageName(lang)}
        </Button>
      </div>
      {machine && target ? <p className="text-xs text-zinc-500">Draft with AI works on the saved version of this message and sends only its text and your glossary.</p> : null}
      {run.data?.status === "failed" ? <InlineError message={`Draft with AI didn't finish: ${run.data.error ?? "unknown reason"}`} /> : null}
      {run.data?.status === "done" ? (
        <div className="flex flex-wrap items-center gap-2 rounded-lg bg-accent-50 px-3 py-2 text-sm text-zinc-700">
          <span>
            {run.data.translated} {run.data.translated === 1 ? "draft is" : "drafts are"} ready.
            {run.data.skipped > 0 ? ` ${run.data.skipped} couldn't be drafted (their template tags didn't come back intact, or the text changed); translate those by hand.` : ""}
          </span>
          <Button size="sm" variant="primary" disabled={readOnly || busy} onClick={addDrafts}>
            Add the drafts
          </Button>
        </div>
      ) : null}
      {error ? <p className="whitespace-pre-line text-sm text-red-600">{error}</p> : null}

      <div className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
        {view.strings.length === 0 ? <p className="p-4 text-sm text-zinc-500">This message has no text to translate yet.</p> : null}
        {view.strings.map((s) => (
          <Row
            key={s.id}
            s={s}
            lang={lang}
            value={edits[s.id] ?? (s.status === "stale" ? "" : s.text)}
            readOnly={readOnly}
            onChange={(text) => setEdits({ ...edits, [s.id]: text })}
            onReview={s.status === "machine" || s.status === "check" ? () => markReviewed([s.id]) : undefined}
            onDraft={machine && target && s.kind !== "link" ? () => draft([s.id]) : undefined}
          />
        ))}
      </div>

      {quote ? (
        <ConfirmDialog
          title="Draft with AI?"
          body={`This sends ${quote.q.characters.toLocaleString()} characters (${quote.q.strings} ${quote.q.strings === 1 ? "text" : "texts"}) for machine translation, for $${quote.q.price_usd.toFixed(2)}.`}
          confirmLabel="Draft them"
          loading={busy}
          onConfirm={() => {
            const ids = quote.ids;
            setQuote(null);
            draft(ids, true);
          }}
          onClose={() => setQuote(null)}
        />
      ) : null}
      {preview ? (
        <Modal title={`Preview in ${languageName(lang)}`} onClose={() => setPreview(null)} size="xl">
          {preview.html ? (
            <>
              <p className="mb-2 text-sm text-zinc-700">
                <span className="text-zinc-500">Subject:</span> {preview.subject}
              </p>
              <iframe title="Preview" srcDoc={preview.html} sandbox="" className="h-[70vh] w-full rounded border border-zinc-200 bg-white" />
            </>
          ) : (
            <p className="whitespace-pre-wrap rounded-lg bg-zinc-100 p-3 text-sm">{preview.body}</p>
          )}
        </Modal>
      ) : null}
    </div>
  );
}

function Row({ s, lang, value, readOnly, onChange, onReview, onDraft }: { s: TranslationString; lang: string; value: string; readOnly?: boolean; onChange: (t: string) => void; onReview?: () => void; onDraft?: () => void }) {
  const st = statusLabel[s.status];
  const long = s.kind === "html" || s.source.length > 80;
  return (
    <div className="grid grid-cols-1 gap-2 p-3 md:grid-cols-2 md:gap-4">
      <div className="min-w-0">
        <div className="mb-1 flex flex-wrap items-center gap-2">
          <span className="text-xs font-medium text-zinc-700">{s.where}</span>
          <span className="text-xs text-zinc-400">{s.kind === "link" ? "link" : s.kind === "html" ? "HTML" : ""}</span>
        </div>
        <p className="whitespace-pre-wrap break-words rounded bg-zinc-50 px-2 py-1.5 text-sm text-zinc-700">{s.source}</p>
      </div>
      <div className="min-w-0">
        <div className="mb-1 flex flex-wrap items-center gap-2">
          <Badge tone={st.tone}>{st.label}</Badge>
          {onReview ? (
            <button type="button" className="text-xs font-medium text-accent-700 hover:underline" onClick={onReview} disabled={readOnly}>
              Mark reviewed
            </button>
          ) : null}
          {onDraft ? (
            <button type="button" className="text-xs font-medium text-zinc-600 hover:underline" onClick={onDraft} disabled={readOnly}>
              Draft with AI
            </button>
          ) : null}
        </div>
        {long ? (
          <Textarea value={value} onChange={(e) => onChange(e.target.value)} rows={s.kind === "html" ? 4 : 2} disabled={readOnly} dir="auto" lang={lang} aria-label={`${s.where} in ${languageName(lang)}`} className={s.kind === "html" ? "font-mono text-xs" : ""} />
        ) : (
          <Input
            value={value}
            onChange={(e) => onChange(e.target.value)}
            disabled={readOnly}
            dir="auto"
            lang={lang}
            placeholder={s.kind === "link" ? "Same link as the default" : s.status === "stale" ? "The original changed; translate it again" : ""}
            aria-label={`${s.where} in ${languageName(lang)}`}
          />
        )}
      </div>
    </div>
  );
}

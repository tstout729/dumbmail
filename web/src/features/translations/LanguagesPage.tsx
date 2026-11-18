import { useState, type FormEvent } from "react";
import { Trash2 } from "lucide-react";
import { Badge, Button, Card, Checkbox, ConfirmDialog, EmptyRow, Field, IconButton, InlineError, Input, Select, Skeleton, Table, TagInput, Td, Th } from "../../components/ui";
import { languageName, useDeleteTerm, useGlossary, useLanguageSettings, useSaveSystemTexts, useSaveTerm, useSystemTexts, useUpdateLanguageSettings } from "./api";
import type { GlossaryTerm, LanguageSettings, SystemTexts } from "./types";

const TAG = /^[A-Za-z]{2,3}([-_][A-Za-z0-9]{2,8})*$/;

/** Settings → Languages: the languages you send in, the glossary, and DumbMail's own wording. */
export function LanguagesPage() {
  const settings = useLanguageSettings();
  if (settings.isPending) return <Skeleton className="h-48 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  return (
    <div className="space-y-4">
      <Languages key={`${settings.data.default_language}-${settings.data.languages.join(",")}`} settings={settings.data} />
      <MachineTranslation settings={settings.data} />
      <Glossary settings={settings.data} />
      <Wording settings={settings.data} />
    </div>
  );
}

function Languages({ settings }: { settings: LanguageSettings }) {
  const update = useUpdateLanguageSettings();
  const [def, setDef] = useState(settings.default_language);
  const [langs, setLangs] = useState(settings.languages);
  const bad = [def, ...langs].filter((t) => !TAG.test(t.trim()));
  const tooMany = langs.length > settings.max_languages;
  const changed = def !== settings.default_language || langs.join(",") !== settings.languages.join(",");
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    update.mutate({ default_language: def.trim(), languages: langs.map((l) => l.trim()) });
  };
  return (
    <form onSubmit={onSubmit}>
      <Card title="Languages you send in">
        <p className="mb-4 text-sm text-zinc-600">
          Each person gets the version of a message in their language, and everyone else gets your default language. Use language tags: <code className="rounded bg-zinc-100 px-1 text-xs">fr</code> for
          French anywhere, <code className="rounded bg-zinc-100 px-1 text-xs">fr-CA</code> for French in Canada, <code className="rounded bg-zinc-100 px-1 text-xs">pt-BR</code> for Brazilian Portuguese.
        </p>
        <div className="grid grid-cols-1 gap-3 md:grid-cols-[12rem_minmax(0,1fr)]">
          <Field label="Default language" hint="What your messages are written in.">
            <Input value={def} onChange={(e) => setDef(e.target.value)} className="w-full" required />
          </Field>
          <Field label="Other languages" hint={`Up to ${settings.max_languages}. Press Enter after each one.`}>
            <TagInput value={langs} onChange={setLangs} placeholder="fr, es, de" aria-label="Other languages" />
          </Field>
        </div>
        {langs.length > 0 ? (
          <p className="mt-3 text-xs text-zinc-500">
            {[def, ...langs]
              .filter((t) => TAG.test(t.trim()))
              .map((t) => `${languageName(t)} (${t})`)
              .join(" · ")}
          </p>
        ) : null}
        {bad.length > 0 ? <p className="mt-2 text-xs text-red-600">{bad.join(", ")} isn't a language tag like fr or pt-BR.</p> : null}
        {tooMany ? <p className="mt-2 text-xs text-red-600">That's more than {settings.max_languages} languages.</p> : null}
        <div className="mt-4">
          <Button type="submit" variant="primary" loading={update.isPending} disabled={!changed || bad.length > 0 || tooMany}>
            Save languages
          </Button>
        </div>
      </Card>
    </form>
  );
}

function MachineTranslation({ settings }: { settings: LanguageSettings }) {
  const m = settings.machine_translation;
  return (
    <Card title="Draft with AI">
      {m.available ? (
        <div className="space-y-1 text-sm text-zinc-600">
          <p>
            In the email editor, Draft with AI fills in missing translations as drafts for someone to review. It sends only the message's text and your glossary, never anything about
            the people you send to.
          </p>
          <p>
            Today: {m.used_today.toLocaleString()} of {m.daily_characters.toLocaleString()} characters used.
            {m.price_per_1000_characters_usd > 0 ? ` $${m.price_per_1000_characters_usd.toFixed(4)} per 1,000 characters.` : " No extra charge."}
          </p>
        </div>
      ) : (
        <p className="text-sm text-zinc-600">Machine translation isn't set up on this server. You can still export a message's text for a translator and import their work.</p>
      )}
    </Card>
  );
}

function Glossary({ settings }: { settings: LanguageSettings }) {
  const glossary = useGlossary();
  const save = useSaveTerm();
  const del = useDeleteTerm();
  const [term, setTerm] = useState("");
  const [never, setNever] = useState(true);
  const [confirm, setConfirm] = useState<GlossaryTerm | null>(null);
  const add = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    save.mutate({ term: term.trim(), never_translate: never }, { onSuccess: () => setTerm("") });
  };
  return (
    <Card title="Glossary" padded={false}>
      <div className="px-4 pt-4 sm:px-5">
        <p className="text-sm text-zinc-600">Words with a fixed translation, and words that are never translated, like your brand name. Machine drafts follow it, and translators see it in exported files.</p>
        <form onSubmit={add} className="mt-3 flex flex-wrap items-end gap-3">
          <Field label="Term">
            <Input value={term} onChange={(e) => setTerm(e.target.value)} maxLength={100} placeholder="Your brand name" className="w-56" />
          </Field>
          <Checkbox label="Never translate" checked={never} onChange={(e) => setNever(e.target.checked)} />
          <Button type="submit" loading={save.isPending} disabled={term.trim() === ""}>
            Add term
          </Button>
        </form>
        <InlineError message={save.error?.message} />
      </div>
      <div className="relative mt-4 overflow-x-auto">
        <Table>
          <thead>
            <tr>
              <Th>Term</Th>
              <Th>Translations</Th>
              <Th className="w-12">
                <span className="sr-only">Actions</span>
              </Th>
            </tr>
          </thead>
          <tbody>
            {glossary.isPending ? null : (glossary.data?.data ?? []).length === 0 ? (
              <EmptyRow cols={3}>No terms yet.</EmptyRow>
            ) : (
              (glossary.data?.data ?? []).map((t) => <TermRow key={t.id} term={t} languages={settings.languages} onDelete={() => setConfirm(t)} />)
            )}
          </tbody>
        </Table>
      </div>
      {confirm ? (
        <ConfirmDialog
          title={`Remove "${confirm.term}"?`}
          body="Machine drafts and exports won't use it any more."
          confirmLabel="Remove"
          danger
          loading={del.isPending}
          onConfirm={() => del.mutate(confirm.id, { onSuccess: () => setConfirm(null) })}
          onClose={() => setConfirm(null)}
        />
      ) : null}
    </Card>
  );
}

function TermRow({ term, languages, onDelete }: { term: GlossaryTerm; languages: string[]; onDelete: () => void }) {
  const save = useSaveTerm();
  const [lang, setLang] = useState(languages[0] ?? "");
  const [text, setText] = useState(term.translations[languages[0] ?? ""] ?? "");
  return (
    <tr>
      <Td className="align-top font-medium text-zinc-900">
        {term.term}
        {term.never_translate ? (
          <span className="ml-2">
            <Badge tone="gray">never translated</Badge>
          </span>
        ) : null}
      </Td>
      <Td className="align-top">
        {term.never_translate || languages.length === 0 ? (
          <span className="text-xs text-zinc-500">{term.never_translate ? "Kept as it is in every language." : "Add a language to give translations."}</span>
        ) : (
          <div className="flex flex-wrap items-center gap-2">
            <Select
              value={lang}
              onChange={(e) => {
                setLang(e.target.value);
                setText(term.translations[e.target.value] ?? "");
              }}
              className="h-8 w-auto text-xs"
              aria-label="Language"
            >
              {languages.map((l) => (
                <option key={l} value={l}>
                  {languageName(l)}
                </option>
              ))}
            </Select>
            <Input value={text} onChange={(e) => setText(e.target.value)} maxLength={200} className="h-8 w-48 text-xs" aria-label={`${term.term} in ${languageName(lang)}`} />
            <Button size="sm" loading={save.isPending} disabled={text === (term.translations[lang] ?? "")} onClick={() => save.mutate({ id: term.id, translations: { ...term.translations, [lang]: text } })}>
              Save
            </Button>
          </div>
        )}
      </Td>
      <Td className="align-top">
        <IconButton label={`Remove ${term.term}`} onClick={onDelete}>
          <Trash2 className="size-4" />
        </IconButton>
      </Td>
    </tr>
  );
}

function Wording({ settings }: { settings: LanguageSettings }) {
  const all = [settings.default_language, ...settings.languages];
  const [lang, setLang] = useState(all[1] ?? all[0]);
  const texts = useSystemTexts(lang);
  return (
    <Card
      title="DumbMail's own pages and footer"
      actions={
        <Select value={lang} onChange={(e) => setLang(e.target.value)} className="h-8 w-auto text-xs" aria-label="Language">
          {all.map((l) => (
            <option key={l} value={l}>
              {languageName(l)}
            </option>
          ))}
        </Select>
      }
    >
      <p className="mb-3 text-sm text-zinc-600">
        The unsubscribe and preference pages, the email that asks people to confirm a sign-up, and the footer lines DumbMail adds to every marketing email, in each person's language. English,
        Spanish, French, German, Italian, Portuguese and Dutch come translated; reword any of them here. Keep <code className="rounded bg-zinc-100 px-1 text-xs">{"{name}"}</code> where your
        name goes.
      </p>
      {texts.isPending ? <Skeleton className="h-40 w-full" /> : texts.error ? <InlineError message={texts.error.message} /> : <WordingForm key={lang} texts={texts.data} />}
    </Card>
  );
}

function WordingForm({ texts }: { texts: SystemTexts }) {
  const save = useSaveSystemTexts();
  const [edits, setEdits] = useState<Record<string, string>>({});
  const changed = Object.keys(edits).length > 0;
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const body: Record<string, string | null> = {};
    for (const [k, v] of Object.entries(edits)) body[k] = v.trim() === "" ? null : v;
    save.mutate({ language: texts.language, texts: body }, { onSuccess: () => setEdits({}) });
  };
  return (
    <form onSubmit={onSubmit} className="space-y-3">
      {!texts.shipped ? <p className="text-xs text-amber-700">DumbMail doesn't come with this language, so these show in English until you word them.</p> : null}
      <div className="divide-y divide-zinc-100" dir={texts.rtl ? "rtl" : undefined}>
        {texts.texts.map((t) => (
          <div key={t.key} className="grid grid-cols-1 gap-1 py-2 md:grid-cols-[16rem_minmax(0,1fr)] md:gap-4">
            <div dir="ltr">
              <p className="text-xs font-medium text-zinc-700">{t.where}</p>
              <p className="text-xs text-zinc-500">{t.english}</p>
            </div>
            <Input
              value={edits[t.key] ?? t.text ?? ""}
              placeholder={t.shipped ?? t.english}
              onChange={(e) => setEdits({ ...edits, [t.key]: e.target.value })}
              maxLength={500}
              aria-label={t.where}
            />
          </div>
        ))}
      </div>
      <InlineError message={save.error?.message} />
      <Button type="submit" variant="primary" loading={save.isPending} disabled={!changed}>
        Save wording
      </Button>
    </form>
  );
}

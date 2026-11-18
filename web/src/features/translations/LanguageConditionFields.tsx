import type { Condition } from "../../api";
import { Select, TagInput } from "../../components/ui";
import { languageName, useLanguageSettings } from "./api";
import type { LanguageCondition } from "./types";

const small = "h-8 w-auto text-xs";

export function newLanguageCondition(): LanguageCondition {
  return { type: "language", op: "is", value: "" };
}

function tags(c: LanguageCondition): string[] {
  return c.op === "is_one_of" ? (c.values ?? []) : c.value ? [c.value] : [];
}

export function isLanguageConditionComplete(c: LanguageCondition): boolean {
  const t = tags(c);
  return t.length > 0 && t.length <= 20 && t.every((x) => /^[A-Za-z]{2,3}([-_][A-Za-z0-9]{2,8})*$/.test(x.trim()));
}

export function languageConditionSummary(c: LanguageCondition): string {
  const names = tags(c).map(languageName);
  if (c.op === "is_one_of") return `language is one of ${names.join(", ")}`;
  return `language ${c.op === "is" ? "is" : "is not"} ${names[0] ?? "…"}`;
}

/** Segment rule: the person's language (fr matches every region of French; fr-CA only Canada). */
export function LanguageConditionFields({ condition: c, onChange }: { condition: LanguageCondition; onChange: (c: Condition) => void }) {
  const settings = useLanguageSettings();
  const known = settings.data ? [settings.data.default_language, ...settings.data.languages] : [];
  const setOp = (op: LanguageCondition["op"]) => {
    const current = tags(c);
    onChange(op === "is_one_of" ? { type: "language", op, values: current } : { type: "language", op, value: current[0] ?? "" });
  };
  return (
    <>
      <Select value={c.op} onChange={(e) => setOp(e.target.value as LanguageCondition["op"])} className={small} aria-label="Language operator">
        <option value="is">is</option>
        <option value="is_not">is not</option>
        <option value="is_one_of">is one of</option>
      </Select>
      {c.op === "is_one_of" ? (
        <div className="min-w-56">
          <TagInput value={c.values ?? []} onChange={(values) => onChange({ ...c, values })} placeholder="fr, es, pt-BR" aria-label="Languages" />
        </div>
      ) : (
        <Select value={c.value ?? ""} onChange={(e) => onChange({ ...c, value: e.target.value })} className={`${small} min-w-40`} aria-label="Language">
          {c.value ? null : <option value="">Choose a language…</option>}
          {[...new Set([...known, ...(c.value ? [c.value] : [])])].map((l) => (
            <option key={l} value={l}>
              {languageName(l)} ({l})
            </option>
          ))}
        </Select>
      )}
    </>
  );
}

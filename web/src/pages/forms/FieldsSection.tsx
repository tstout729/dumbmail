import { useState } from "react";
import { AlertTriangle, ArrowDown, ArrowUp, ChevronDown, ListChecks, MessageSquareText, Pencil, Plus, Trash2, X } from "lucide-react";
import type { CustomFieldType, FormField, FormFieldDef, PhoneStep, StandardFieldKey } from "../../api";
import {
  Badge,
  Button,
  Checkbox,
  CollapsibleCard,
  Field,
  IconButton,
  Input,
  Menu,
  Notice,
  Select,
  SubHeading,
  Textarea,
  Toggle,
  cx,
} from "../../components/ui";
import {
  contentDefaults,
  customFieldTypes,
  fieldProblems,
  fieldsOf,
  hasPhoneField,
  isStandardKey,
  keyFromLabel,
  maxFields,
  needsOptions,
  phoneStepDefaults,
  standardFields,
} from "./formConfig";
import { sharedText, type Editor } from "./editorShared";

type SectionProps = { ed: Editor; open: boolean; onToggle: () => void };

const fieldName = (f: FormFieldDef) => f.label || (isStandardKey(f.key) ? standardFields[f.key].label : f.key);
const isRequired = (f: FormFieldDef) => f.required ?? f.key === "email";
const legacyKeys: FormField[] = ["email", "phone_number", "first_name"];

/** A key not used by another field: skin_type, skin_type_2, … */
function uniqueKey(base: string, fields: FormFieldDef[], skip?: number) {
  const taken = new Set(fields.filter((_, i) => i !== skip).map((f) => f.key));
  if (!taken.has(base)) return base;
  for (let n = 2; ; n++) {
    const k = `${base.slice(0, 37)}_${n}`;
    if (!taken.has(k)) return k;
  }
}

export function FieldsSection({ ed, open, onToggle }: SectionProps) {
  const fields = fieldsOf(ed.config);
  const [editing, setEditing] = useState<number | null>(null);
  // Writing `fields` replaces the legacy `collect` list; keep `collect` in step so older readers agree.
  const setFields = (next: FormFieldDef[]) => {
    const collect = legacyKeys.filter((k) => next.some((f) => f.key === k));
    ed.set({ fields: next, collect: collect.length ? collect : undefined });
  };
  const update = (i: number, patch: Partial<FormFieldDef>) => setFields(fields.map((f, j) => (j === i ? { ...f, ...patch } : f)));
  const move = (i: number, by: -1 | 1) => {
    const next = [...fields];
    [next[i], next[i + by]] = [next[i + by], next[i]];
    setFields(next);
    if (editing === i) setEditing(i + by);
    else if (editing === i + by) setEditing(i);
  };
  const remove = (i: number) => {
    setFields(fields.filter((_, j) => j !== i));
    setEditing(null);
  };
  const add = (f: FormFieldDef) => {
    setFields([...fields, f]);
    setEditing(fields.length);
  };
  const missingStandard = (Object.keys(standardFields) as StandardFieldKey[]).filter((k) => !fields.some((f) => f.key === k));
  const problems = fieldProblems(fields);
  const phone = hasPhoneField(ed.config);

  return (
    <CollapsibleCard
      title="Fields"
      icon={<ListChecks />}
      summary={fields.map(fieldName).join(", ")}
      open={open}
      onToggle={onToggle}
    >
      {problems.length ? (
        <Notice tone="amber" icon={<AlertTriangle />} title="Fix before saving">
          <ul className="list-disc pl-4">
            {problems.map((p) => (
              <li key={p}>{p}</li>
            ))}
          </ul>
        </Notice>
      ) : null}
      <ol className="space-y-2">
        {fields.map((f, i) => (
          <li key={i} className={cx("rounded-md border bg-white", editing === i ? "border-zinc-300 shadow-xs" : "border-zinc-200")}>
            <div className="flex items-center gap-1 py-1 pl-3 pr-1">
              <button type="button" className="flex min-w-0 flex-1 items-center gap-2 py-1 text-left" onClick={() => setEditing(editing === i ? null : i)}>
                <span className="truncate text-sm font-medium text-zinc-900">{fieldName(f)}</span>
                {isStandardKey(f.key) ? null : <Badge>{customFieldTypes.find((t) => t.value === (f.type ?? "text"))?.label ?? "Custom"}</Badge>}
                {isRequired(f) ? <span className="text-xs text-zinc-500">Required</span> : null}
              </button>
              <IconButton size="sm" label="Move up" disabled={i === 0} onClick={() => move(i, -1)}>
                <ArrowUp className="size-3.5" />
              </IconButton>
              <IconButton size="sm" label="Move down" disabled={i === fields.length - 1} onClick={() => move(i, 1)}>
                <ArrowDown className="size-3.5" />
              </IconButton>
              <IconButton size="sm" label={editing === i ? "Done editing" : "Edit field"} onClick={() => setEditing(editing === i ? null : i)}>
                {editing === i ? <ChevronDown className="size-3.5 rotate-180" /> : <Pencil className="size-3.5" />}
              </IconButton>
              <IconButton size="sm" label="Remove field" onClick={() => remove(i)} className="hover:text-red-700">
                <Trash2 className="size-3.5" />
              </IconButton>
            </div>
            {editing === i ? <FieldEditor field={f} fields={fields} index={i} onChange={(patch) => update(i, patch)} /> : null}
          </li>
        ))}
      </ol>
      {fields.length < maxFields ? (
        <Menu
          align="left"
          trigger={(p) => (
            <Button size="sm" icon={<Plus className="size-3.5" />} {...p}>
              Add field
            </Button>
          )}
          items={[
            ...missingStandard.map((k) => ({ label: standardFields[k].label, onSelect: () => add({ key: k }) })),
            {
              label: "Custom question…",
              onSelect: () => add({ key: uniqueKey("question", fields), type: "text" as const, label: "Question" }),
            },
          ]}
        />
      ) : (
        <p className="text-xs text-zinc-500">A form can have up to {maxFields} fields.</p>
      )}
      {phone ? <SmsConsentField ed={ed} /> : null}
    </CollapsibleCard>
  );
}

function SmsConsentField({ ed }: { ed: Editor }) {
  return (
    <>
      <Field label="SMS consent text" hint="Shown next to the consent checkbox under the phone field. Carriers require clear consent language.">
        <Textarea rows={3} maxLength={1000} {...sharedText(ed, "sms_consent_text", contentDefaults.sms_consent_text)} />
      </Field>
      <Field label="WhatsApp consent text (optional)" hint="Adds a second box under the phone field. Ticking it signs the number up for WhatsApp marketing. Leave empty for no box.">
        <Textarea rows={2} maxLength={1000} {...sharedText(ed, "whatsapp_consent_text")} placeholder="Also send me offers on WhatsApp. Reply STOP to unsubscribe." />
      </Field>
    </>
  );
}

type FieldEditorProps = { field: FormFieldDef; fields: FormFieldDef[]; index: number; onChange: (patch: Partial<FormFieldDef>) => void };

function FieldEditor({ field: f, fields, index, onChange }: FieldEditorProps) {
  const standard = isStandardKey(f.key);
  const type = f.type ?? "text";
  const defaults = standard ? standardFields[f.key as StandardFieldKey] : null;
  const hasPlaceholder = type !== "checkbox" && !needsOptions(type);
  const setLabel = (label: string) => {
    // A custom field's saved-as name follows its label until someone edits the name by hand.
    const follows = !standard && f.key === uniqueKey(keyFromLabel(f.label ?? ""), fields, index);
    onChange({ label: label || undefined, ...(follows && label ? { key: uniqueKey(keyFromLabel(label), fields, index) } : {}) });
  };
  return (
    <div className="space-y-3 border-t border-zinc-100 px-3 pb-3 pt-3">
      {standard ? null : (
        <Field label="Answer type">
          <Select
            value={type}
            onChange={(e) => {
              const next = e.target.value as CustomFieldType;
              onChange({ type: next, options: needsOptions(next) ? (f.options?.length ? f.options : ["Option 1", "Option 2"]) : undefined });
            }}
          >
            {customFieldTypes.map((t) => (
              <option key={t.value} value={t.value}>
                {t.label}
              </option>
            ))}
          </Select>
        </Field>
      )}
      <div className={cx("grid grid-cols-1 gap-3", hasPlaceholder && "sm:grid-cols-2")}>
        <Field label={type === "checkbox" ? "Checkbox text" : "Label"} hint={standard ? "Empty shows just the placeholder." : undefined}>
          <Input maxLength={100} value={f.label ?? ""} placeholder={defaults?.label ?? "Question"} onChange={(e) => setLabel(e.target.value)} />
        </Field>
        {!hasPlaceholder ? null : (
          <Field label="Placeholder">
            <Input maxLength={100} value={f.placeholder ?? ""} placeholder={defaults?.placeholder ?? ""} onChange={(e) => onChange({ placeholder: e.target.value || undefined })} />
          </Field>
        )}
      </div>
      {needsOptions(type) ? <OptionsEditor options={f.options ?? []} onChange={(options) => onChange({ options })} /> : null}
      <Checkbox
        label="Required"
        description={type === "checkbox" ? "They must tick it to sign up." : undefined}
        checked={isRequired(f)}
        onChange={(e) => onChange({ required: e.target.checked })}
      />
      {standard ? (
        <p className="text-xs text-zinc-500">
          {f.key === "phone_number"
            ? "Saves the phone number, plus SMS consent when they tick the consent checkbox."
            : f.key === "birthday"
              ? "Saved on the profile as birthday."
              : `Saved on the profile's ${standardFields[f.key as StandardFieldKey].label.toLowerCase()}.`}{" "}
          Sign-ups never overwrite what a profile already has.
        </p>
      ) : (
        <Field label="Saved on the profile as" hint="Lowercase letters, numbers and _. Use it in segments and personalization.">
          <Input
            maxLength={40}
            className="font-mono"
            value={f.key}
            onChange={(e) => onChange({ key: e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, "_") })}
          />
        </Field>
      )}
    </div>
  );
}

function OptionsEditor({ options, onChange }: { options: string[]; onChange: (options: string[]) => void }) {
  return (
    <div className="space-y-1.5">
      <div className="text-xs font-medium text-zinc-700">Options</div>
      {options.map((o, i) => (
        <div key={i} className="flex items-center gap-1.5">
          <Input
            value={o}
            maxLength={80}
            aria-label={`Option ${i + 1}`}
            onChange={(e) => onChange(options.map((x, j) => (j === i ? e.target.value : x)))}
            className="min-w-0 flex-1"
          />
          <IconButton size="sm" label="Remove option" disabled={options.length <= 1} onClick={() => onChange(options.filter((_, j) => j !== i))}>
            <X className="size-3.5" />
          </IconButton>
        </div>
      ))}
      {options.length < 20 ? (
        <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => onChange([...options, `Option ${options.length + 1}`])}>
          Add option
        </Button>
      ) : null}
    </div>
  );
}

export function PhoneStepSection({ ed, open, onToggle }: SectionProps) {
  const s: PhoneStep = ed.config.phone_step ?? {};
  const enabled = s.enabled ?? false;
  const phoneField = hasPhoneField(ed.config);
  const set = (patch: Partial<PhoneStep>) => ed.set({ phone_step: { ...s, ...patch } });
  const text = (key: Exclude<keyof PhoneStep, "enabled">) => ({
    value: s[key] ?? "",
    placeholder: phoneStepDefaults[key],
    onChange: (e: { target: { value: string } }) => set({ [key]: e.target.value || undefined }),
  });
  return (
    <CollapsibleCard
      title="Ask for a phone number next"
      icon={<MessageSquareText />}
      summary={phoneField ? "Not needed: the form has a phone field" : enabled ? "On: a second screen asks for a phone number" : "Off"}
      open={open}
      onToggle={onToggle}
    >
      <div className="flex items-start justify-between gap-3">
        <p className="text-sm text-zinc-600">After someone signs up with their email, a second screen asks for their phone number for texts.</p>
        <Toggle checked={enabled} onChange={(on) => set({ enabled: on })} label="Ask for a phone number next" />
      </div>
      {phoneField ? (
        <p className="text-xs text-amber-700">This form already asks for a phone number, so the step is skipped.</p>
      ) : enabled ? (
        <>
          <Field label="Headline">
            <Input maxLength={200} {...text("headline")} />
          </Field>
          <Field label="Text">
            <Textarea rows={2} maxLength={1000} {...text("body")} />
          </Field>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <Field label="Button">
              <Input maxLength={60} {...text("button_text")} />
            </Field>
            <Field label="Skip link">
              <Input maxLength={60} {...text("skip_text")} />
            </Field>
          </div>
          <SubHeading>Consent</SubHeading>
          <SmsConsentField ed={ed} />
        </>
      ) : null}
    </CollapsibleCard>
  );
}

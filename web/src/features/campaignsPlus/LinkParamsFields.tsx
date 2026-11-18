import { Plus, Trash2 } from "lucide-react";
import { Button, Field, IconButton, Input } from "../../components/ui";
import type { LinkParams, LinkVariable } from "./types";

const STANDARD: { key: "source" | "medium" | "campaign" | "content" | "term"; label: string }[] = [
  { key: "source", label: "utm_source" },
  { key: "medium", label: "utm_medium" },
  { key: "campaign", label: "utm_campaign" },
  { key: "content", label: "utm_content" },
  { key: "term", label: "utm_term" },
];

export const emptyParams: LinkParams = { custom: [] };

type Props = {
  value: LinkParams;
  onChange: (p: LinkParams) => void;
  variables: LinkVariable[];
  /** What an empty field means, per field. */
  fallbacks: Partial<Record<"source" | "medium" | "campaign" | "content" | "term", string>>;
  disabled?: boolean;
};

/** The UTM values and custom parameters, with the variables they may use. */
export function LinkParamsFields({ value, onChange, variables, fallbacks, disabled }: Props) {
  const custom = value.custom ?? [];
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {STANDARD.map(({ key, label }) => (
          <Field key={key} label={<span className="font-mono">{label}</span>} hint={fallbacks[key] ? `Empty: ${fallbacks[key]}` : "Empty: left out"}>
            <Input value={value[key] ?? ""} maxLength={200} disabled={disabled} onChange={(e) => onChange({ ...value, [key]: e.target.value })} className="font-mono" />
          </Field>
        ))}
      </div>
      <div>
        <div className="mb-1.5 text-sm font-medium text-zinc-800">Custom parameters</div>
        {custom.length === 0 ? <p className="mb-2 text-xs text-zinc-500">None. Add up to 10, like ref=newsletter.</p> : null}
        <div className="space-y-2">
          {custom.map((c, i) => (
            <div key={i} className="flex items-center gap-2">
              <Input
                aria-label="Parameter name"
                placeholder="name"
                value={c.name}
                maxLength={40}
                disabled={disabled}
                className="w-40 font-mono"
                onChange={(e) => onChange({ ...value, custom: custom.map((x, j) => (j === i ? { ...x, name: e.target.value.toLowerCase() } : x)) })}
              />
              <span className="text-zinc-400">=</span>
              <Input
                aria-label="Parameter value"
                placeholder="value"
                value={c.value}
                maxLength={200}
                disabled={disabled}
                className="min-w-0 flex-1 font-mono"
                onChange={(e) => onChange({ ...value, custom: custom.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)) })}
              />
              <IconButton label="Remove parameter" disabled={disabled} onClick={() => onChange({ ...value, custom: custom.filter((_, j) => j !== i) })}>
                <Trash2 className="size-3.5" />
              </IconButton>
            </div>
          ))}
        </div>
        {custom.length < 10 ? (
          <Button size="sm" variant="ghost" className="mt-2" icon={<Plus className="size-3.5" />} disabled={disabled} onClick={() => onChange({ ...value, custom: [...custom, { name: "", value: "" }] })}>
            Add parameter
          </Button>
        ) : null}
      </div>
      <details className="text-xs text-zinc-600">
        <summary className="cursor-pointer select-none font-medium text-zinc-700">Variables you can use</summary>
        <ul className="mt-2 space-y-1">
          {variables.map((v) => (
            <li key={v.name}>
              <code className="rounded bg-zinc-100 px-1 font-mono">{`{${v.name}}`}</code> {v.description}
            </li>
          ))}
        </ul>
      </details>
    </div>
  );
}

import { Plus, X } from "lucide-react";
import type { FieldFilter } from "../../api/types";
import { FilterFields } from "../../components/segments/ConditionBuilder";
import { Button, IconButton } from "../../components/ui";
import type { FieldDef } from "./types";

/** Rules on an object's fields (segment rules, trigger filters, condition splits). */
export function ObjectFilters({ filters, onChange, fields, listId, addLabel = "Add field rule", compact }: { filters: FieldFilter[]; onChange: (f: FieldFilter[]) => void; fields: FieldDef[]; listId: string; addLabel?: string; compact?: boolean }) {
  const first = fields.find((f) => f.type === "date" || f.type === "datetime") ?? fields[0];
  return (
    <div className={compact ? "w-full space-y-1.5" : "space-y-2"}>
      <datalist id={listId}>
        {fields.map((f) => (
          <option key={f.key} value={f.key}>
            {f.label ?? f.key}
          </option>
        ))}
      </datalist>
      {filters.map((f, i) => (
        <div key={i} className={compact ? "flex flex-wrap items-center gap-1.5 pl-2" : "flex flex-wrap items-center gap-1.5 rounded-md border border-zinc-200 p-2"}>
          {compact ? <span className="text-xs text-zinc-500">where</span> : null}
          <FilterFields filter={f} onChange={(nf) => onChange(filters.map((x, j) => (j === i ? nf : x)))} fieldPlaceholder="field, e.g. status" fieldList={listId} />
          <IconButton label="Remove rule" size="sm" onClick={() => onChange(filters.filter((_, j) => j !== i))}>
            <X className="size-3" />
          </IconButton>
        </div>
      ))}
      {compact ? (
        <button type="button" onClick={() => onChange([...filters, first ? { field: first.key, op: first.type === "date" || first.type === "datetime" ? "within_next_days" : "eq", value: first.type === "date" || first.type === "datetime" ? 14 : "" } : { field: "", op: "eq", value: "" }])} className="pl-2 text-xs font-medium text-accent-700 hover:underline">
          + {addLabel}
        </button>
      ) : (
        <Button size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => onChange([...filters, { field: first?.key ?? "", op: "eq", value: "" }])}>
          {addLabel}
        </Button>
      )}
    </div>
  );
}

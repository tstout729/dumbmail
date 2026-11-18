import type { Condition } from "../../api";
import { Input, Select } from "../../components/ui";
import { useObjectTypes } from "./api";
import { ObjectFilters } from "./ObjectFilters";
import type { ObjectCondition } from "./types";

const small = "h-8 w-auto text-xs";

/** Segment rule: has (or doesn't have) objects of a type whose fields match. */
export function ObjectConditionFields({ condition: c, onChange }: { condition: ObjectCondition; onChange: (c: Condition) => void }) {
  const types = useObjectTypes();
  const list = types.data?.data ?? [];
  const ty = list.find((t) => t.key === c.object_type);
  return (
    <>
      <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as ObjectCondition["op"] })} className={small} aria-label="How many">
        <option value="at_least">has at least</option>
        <option value="at_most">has at most</option>
        <option value="exactly">has exactly</option>
        <option value="zero">has no</option>
      </Select>
      {c.op !== "zero" ? (
        <Input type="number" min={0} value={c.count ?? 1} onChange={(e) => onChange({ ...c, count: Math.max(0, Number(e.target.value) || 0) })} className="h-8 w-16 text-xs" aria-label="Count" />
      ) : null}
      <Select value={c.object_type} onChange={(e) => onChange({ ...c, object_type: e.target.value, where: [] })} className={`${small} min-w-36`} aria-label="Object type">
        {c.object_type === "" ? <option value="">{list.length ? "Choose a type…" : "No object types yet"}</option> : null}
        {list.map((t) => (
          <option key={t.id} value={t.key}>
            {t.plural.toLowerCase()}
          </option>
        ))}
      </Select>
      <Input
        value={c.relationship ?? ""}
        onChange={(e) => onChange({ ...c, relationship: e.target.value.trim() ? e.target.value : undefined })}
        placeholder="as any role"
        className="h-8 w-28 text-xs"
        aria-label="Relationship (e.g. guest)"
      />
      <ObjectFilters filters={c.where ?? []} onChange={(where) => onChange({ ...c, where })} fields={(ty?.fields ?? []).filter((f) => !f.sensitive)} listId={`pl-object-fields-${c.object_type || "none"}`} compact />
    </>
  );
}

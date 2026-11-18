import type { FieldFilter } from "../../api/types";
import { Checkbox } from "../../components/ui";
import { useObjectTypes } from "./api";
import { ObjectFilters } from "./ObjectFilters";

/** A condition split's rules on the flow's object (object-triggered flows only). */
export function ObjectSplitFields({ objectType, filters, onChange }: { objectType: string; filters: FieldFilter[] | undefined; onChange: (f: FieldFilter[] | undefined) => void }) {
  const types = useObjectTypes();
  // Rules can't test sensitive fields, so they aren't offered.
  const fields = (types.data?.data ?? []).find((t) => t.key === objectType)?.fields.filter((f) => !f.sensitive) ?? [];
  return (
    <div className="space-y-2 rounded-md border border-zinc-200 p-3">
      <Checkbox
        label="Also test the flow's object"
        description="Its fields must match too, as they are when this step runs."
        checked={filters !== undefined}
        onChange={(e) => onChange(e.target.checked ? [{ field: fields[0]?.key ?? "", op: "eq", value: "" }] : undefined)}
      />
      {filters !== undefined ? <ObjectFilters filters={filters} onChange={onChange} fields={fields} listId="pl-split-object-fields" addLabel="Add object rule" /> : null}
    </div>
  );
}

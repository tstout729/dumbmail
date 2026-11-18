import type { FieldFilter } from "../../api/types";
import { Checkbox } from "../../components/ui";
import { ObjectFilters } from "./ObjectFilters";

/** A condition split's rules on the flow's object (object-triggered flows only). */
export function ObjectSplitFields({ filters, onChange }: { filters: FieldFilter[] | undefined; onChange: (f: FieldFilter[] | undefined) => void }) {
  return (
    <div className="space-y-2 rounded-md border border-zinc-200 p-3">
      <Checkbox
        label="Also test the flow's object"
        description="In flows started by an object (e.g. a reservation): its fields must match too. Elsewhere this is never true."
        checked={filters !== undefined}
        onChange={(e) => onChange(e.target.checked ? [{ field: "", op: "eq", value: "" }] : undefined)}
      />
      {filters !== undefined ? <ObjectFilters filters={filters} onChange={onChange} fields={[]} listId="pl-split-object-fields" addLabel="Add object rule" /> : null}
    </div>
  );
}

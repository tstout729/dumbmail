import { Segmented } from "../ui";

export type RangeDays = 7 | 30 | 90;

export function RangePicker({ value, onChange }: { value: RangeDays; onChange: (v: RangeDays) => void }) {
  return (
    <Segmented
      value={String(value) as "7" | "30" | "90"}
      onChange={(v) => onChange(Number(v) as RangeDays)}
      options={[
        { value: "7", label: "7 days" },
        { value: "30", label: "30 days" },
        { value: "90", label: "90 days" },
      ]}
    />
  );
}

export function parseRange(v: string | null): RangeDays {
  return v === "7" ? 7 : v === "90" ? 90 : 30;
}

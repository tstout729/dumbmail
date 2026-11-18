import { Field, Input } from "../../components/ui";

/** The flow's daily limit on notification emails (blank: 200). */
export function NotifyCapField({ value, onChange }: { value: number | undefined; onChange: (v: number | undefined) => void }) {
  const bad = value !== undefined && (!Number.isInteger(value) || value < 1 || value > 2000);
  return (
    <Field label="Notifications per day" hint="Most notification emails this flow sends in a day, so a broken trigger can't flood anyone. Blank = 200, at most 2,000." error={bad ? "Enter a whole number from 1 to 2,000." : null}>
      <Input type="number" min={1} max={2000} value={value ?? ""} placeholder="200" onChange={(e) => onChange(e.target.value === "" ? undefined : Number(e.target.value))} className="w-28" />
    </Field>
  );
}

import { useState, type FormEvent } from "react";
import { useSession } from "../../api";
import { Button, Card, ColorInput, Field, Input, Select, Textarea } from "../../components/ui";
import { timeZones } from "../../lib/tz";
import { formString, useSaveAccount } from "./shared";

export function AccountSettingsPage() {
  const { account } = useSession();
  const { save, saving } = useSaveAccount();
  const [color, setColor] = useState(account.brand_color);
  const zones = timeZones();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    save({
      name: formString(d, "name") ?? account.name,
      website: formString(d, "website"),
      timezone: String(d.get("timezone")),
      address: formString(d, "address"),
      logo_url: formString(d, "logo_url"),
      brand_color: color,
      onboarding: { brand: true },
    });
  };
  return (
    <form onSubmit={onSubmit}>
      <Card title="Account">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Company name">
            <Input name="name" required defaultValue={account.name} />
          </Field>
          <Field label="Website">
            <Input name="website" type="text" inputMode="url" autoComplete="url" defaultValue={account.website ?? ""} placeholder="acme.com" />
          </Field>
          <Field label="Time zone" hint="Used for scheduling, reports and date boundaries.">
            <Select name="timezone" defaultValue={account.timezone}>
              {(zones.includes(account.timezone) ? zones : [account.timezone, ...zones]).map((z) => (
                <option key={z} value={z}>
                  {z.replace(/_/g, " ")}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Brand color">
            <ColorInput value={color} onChange={setColor} />
          </Field>
          <Field label="Logo URL" className="sm:col-span-2">
            <Input name="logo_url" defaultValue={account.logo_url ?? ""} placeholder="https://…/logo.png" />
          </Field>
          <Field
            label="Mailing address"
            className="sm:col-span-2"
            hint="Required by CAN-SPAM. It's added to the footer of every marketing email. Put the street, city, state and ZIP on separate lines or separate them with commas."
          >
            <Textarea name="address" rows={2} defaultValue={account.address ?? ""} />
          </Field>
        </div>
        <div className="mt-4 flex justify-end">
          <Button type="submit" variant="primary" loading={saving}>
            Save
          </Button>
        </div>
      </Card>
    </form>
  );
}

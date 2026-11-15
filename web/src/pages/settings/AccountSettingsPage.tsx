import { useState, type FormEvent } from "react";
import { Sparkles } from "lucide-react";
import { socialLabels, socialNetworks, useImportBrand, useSession, type BrandKit } from "../../api";
import { ColorSwatches, LogoThumb } from "../../components/brand/BrandParts";
import { Button, Card, ColorInput, Field, InlineError, Input, Notice, Select, Textarea } from "../../components/ui";
import { timeZones } from "../../lib/tz";
import { formString, useSaveAccount } from "./shared";

type Seed = { name?: string; website?: string; logo_url?: string; address?: string; social?: Partial<Record<string, string>> };

export function AccountSettingsPage() {
  const { account } = useSession();
  const { save, saving } = useSaveAccount();
  const importer = useImportBrand();
  const [color, setColor] = useState(account.brand_color);
  const [logo, setLogo] = useState(account.logo_url ?? "");
  // Imported values pre-fill the form; bumping `formKey` remounts the inputs with them.
  const [seed, setSeed] = useState<Seed>({});
  const [formKey, setFormKey] = useState(0);
  const [found, setFound] = useState<BrandKit | null>(null);
  const [site, setSite] = useState(account.website ?? "");
  const zones = timeZones();

  const runImport = () =>
    importer.mutate(site, {
      onSuccess: (kit) => {
        setFound(kit);
        if (kit.brand_color) setColor(kit.brand_color);
        if (kit.logo_url) setLogo(kit.logo_url);
        setSite(kit.website);
        setSeed({
          website: kit.website,
          // Keep a company name and address the user already set; fill them only when empty.
          address: account.address ? undefined : (kit.address ?? undefined),
          social: { ...account.social_links, ...kit.social_links },
        });
        setFormKey((k) => k + 1);
      },
    });

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const social: Record<string, string> = {};
    for (const n of socialNetworks) social[n] = formString(d, `social_${n}`) ?? "";
    save({
      name: formString(d, "name") ?? account.name,
      website: site.trim() || null,
      timezone: String(d.get("timezone")),
      address: formString(d, "address"),
      logo_url: logo.trim() || null,
      brand_color: color,
      social_links: social,
      onboarding: { brand: true },
    });
    setFound(null);
  };

  return (
    <form onSubmit={onSubmit} key={formKey}>
      <Card title="Account">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="Website" className="sm:col-span-2" hint="Import reads your site's logo, colors, address and social links into this form.">
            <div className="flex gap-2">
              <Input
                className="min-w-0 flex-1"
                name="website"
                type="text"
                inputMode="url"
                autoComplete="url"
                value={site}
                onChange={(e) => setSite(e.target.value)}
                placeholder="acme.com"
              />
              <Button type="button" variant="secondary" icon={<Sparkles className="size-4" />} loading={importer.isPending} disabled={!site.trim()} onClick={runImport}>
                Import
              </Button>
            </div>
            <div className="mt-2 empty:hidden">
              <InlineError message={importer.error?.message} />
            </div>
          </Field>
          {found ? (
            <div className="sm:col-span-2">
              <Notice tone="blue" icon={<Sparkles />} title="Filled in from your site">
                Check the logo, color, address and social links below, then save.
              </Notice>
            </div>
          ) : null}
          <Field label="Company name">
            <Input name="name" required defaultValue={seed.name ?? account.name} />
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
          <Field label="Logo" className="sm:col-span-2" hint="A PNG or JPG link shows in every inbox.">
            <div className="flex items-center gap-3">
              <LogoThumb url={logo} />
              <Input className="min-w-0 flex-1" name="logo_url" value={logo} onChange={(e) => setLogo(e.target.value)} placeholder="https://…/logo.png" />
            </div>
          </Field>
          <Field label="Brand color" className="sm:col-span-2">
            <div className="flex flex-wrap items-center gap-3">
              <ColorSwatches colors={found?.colors ?? []} value={color} onPick={setColor} />
              <ColorInput value={color} onChange={setColor} />
            </div>
          </Field>
          <Field
            label="Mailing address"
            className="sm:col-span-2"
            hint="Required by CAN-SPAM. It's added to the footer of every marketing email. Put the street, city, state and ZIP on separate lines or separate them with commas."
          >
            <Textarea name="address" rows={2} defaultValue={seed.address ?? account.address ?? ""} />
          </Field>
        </div>
        <h3 className="mt-6 mb-3 text-sm font-semibold text-zinc-900">Social links</h3>
        <p className="-mt-2 mb-3 text-xs text-zinc-500">Shown in the footer of your emails. Leave blank to hide.</p>
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          {socialNetworks.map((n) => (
            <Field key={n} label={socialLabels[n]}>
              <Input
                name={`social_${n}`}
                type="text"
                inputMode="url"
                defaultValue={seed.social?.[n] ?? account.social_links?.[n] ?? ""}
                placeholder={`https://${n === "x" ? "x" : n}.com/yourbrand`}
              />
            </Field>
          ))}
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

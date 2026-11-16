import { Plus, Trophy } from "lucide-react";
import type { ReactNode } from "react";
import type { Form, FormConfig, FormVariant, FormVariantField, VersionId } from "../../api";
import { Badge, Button, Card, Segmented, Table, Td, Th, cx } from "../../components/ui";
import { formatNumber, formatPercent } from "../../lib/format";

export type VariantId = FormVariant["id"];

/** Fields a version can change. Everything else in the config is shared by all versions. */
export const variantFields: FormVariantField[] = ["headline", "body", "button_text", "image_url"];
const variantIds: VariantId[] = ["b", "c", "d"];

export const versionLabel = (id: VersionId) => `Version ${id.toUpperCase()}`;

/** The versions in a test: A plus each variant in order, or none when no test is set up. */
export function versionIds(config: FormConfig): VersionId[] {
  const vs = config.variants ?? [];
  return vs.length ? ["a", ...vs.map((v) => v.id)] : [];
}

/** The config as `version` shows it: the variant's non-empty fields over the base, as the server renders it. */
export function versionConfig(config: FormConfig, version: VersionId): FormConfig {
  const v = config.variants?.find((x) => x.id === version);
  if (!v) return config;
  const out: FormConfig = { ...config };
  for (const k of variantFields) {
    const val = v[k];
    if (val && val.trim()) out[k] = val;
  }
  return out;
}

/** A new version starts as a copy of A's content. */
export function newVariant(config: FormConfig, id: VariantId): FormVariant {
  return { id, headline: config.headline, body: config.body, button_text: config.button_text, image_url: config.image_url };
}

/** The variant id the next added version gets, or null at the limit (D). */
export function nextVariantId(config: FormConfig): VariantId | null {
  return variantIds[(config.variants ?? []).length] ?? null;
}

/** Drops one version and re-letters the rest so ids stay b, c, d in order. */
export function removeVariant(variants: FormVariant[], id: VariantId): FormVariant[] {
  return variants.filter((v) => v.id !== id).map((v, i) => ({ ...v, id: variantIds[i] }));
}

/** Card title with an "All versions" tag while a test runs, so shared settings read as shared. */
export function SharedTitle({ testing, children }: { testing: boolean; children: ReactNode }) {
  return (
    <span className="flex items-center gap-2">
      {children}
      {testing ? <Badge>All versions</Badge> : null}
    </span>
  );
}

type AbTestCardProps = {
  config: FormConfig;
  /** The saved config, to tell the person when ending or starting the test still needs a save. */
  savedConfig: FormConfig;
  onStart: () => void;
  onAdd: () => void;
  onEnd: () => void;
};

export function AbTestCard({ config, savedConfig, onStart, onAdd, onEnd }: AbTestCardProps) {
  const versions = versionIds(config);
  const savedTesting = versionIds(savedConfig).length > 0;
  if (!versions.length) {
    return (
      <Card title="A/B test">
        <p className="text-sm text-zinc-600">Try different headlines, text, button labels or images and see which gets more sign-ups.</p>
        {savedTesting ? (
          // Starting again before the end is saved would carry the old test's counts into the new one.
          <p className="mt-2 text-xs text-amber-700">The test ends when you save. Then you can start a new one.</p>
        ) : (
          <Button className="mt-3" icon={<Plus className="size-3.5" />} onClick={onStart}>
            Start A/B test
          </Button>
        )}
      </Card>
    );
  }
  const next = nextVariantId(config);
  return (
    <Card
      title={
        <span className="flex items-center gap-2">
          A/B test <Badge tone="violet">{versions.length} versions</Badge>
        </span>
      }
      actions={
        <Button size="sm" variant="ghost" onClick={onEnd}>
          End test
        </Button>
      }
    >
      <div className="space-y-3 text-sm text-zinc-600">
        <p>
          Each visitor gets one version at random and keeps seeing it. Versions differ only in headline, body, button text and image. Fields, colors, targeting
          and the success message are shared.
        </p>
        {!savedTesting ? <p className="text-xs text-amber-700">Save to start the test.</p> : null}
        {next ? (
          <Button size="sm" icon={<Plus className="size-3.5" />} onClick={onAdd}>
            Add version {next.toUpperCase()}
          </Button>
        ) : (
          <p className="text-xs text-zinc-500">Four versions is the most a test can have.</p>
        )}
      </div>
    </Card>
  );
}

export function VersionTabs({ versions, value, onChange }: { versions: VersionId[]; value: VersionId; onChange: (v: VersionId) => void }) {
  return <Segmented size="sm" value={value} onChange={onChange} options={versions.map((v) => ({ value: v, label: v.toUpperCase() }))} />;
}

type ResultsProps = {
  form: Form;
  /** Versions whose unsaved content still matches what was counted; only these can be picked. */
  versions: VersionId[];
  onUse: (id: VersionId) => void;
};

/** Per-version views and sign-ups from the server while a saved test runs. */
export function AbResultsCard({ form, versions, onUse }: ResultsProps) {
  const rows = form.variant_stats;
  if (!rows.length) {
    return (
      <Card title="A/B test results">
        <p className="text-sm text-zinc-500">
          {form.status === "live" ? "Save to start counting views and sign-ups for each version." : "Save and publish the form to start counting views and sign-ups for each version."}
        </p>
      </Card>
    );
  }
  const seen = rows.filter((r) => r.views > 0 && r.submits > 0);
  const top = Math.max(0, ...seen.map((r) => r.submit_rate));
  const leaders = seen.filter((r) => r.submit_rate === top);
  const best = leaders.length === 1 ? leaders[0].id : null;
  return (
    <Card title="A/B test results" padded={false}>
      <Table className="rounded-none border-0 shadow-none">
        <thead>
          <tr>
            <Th>Version</Th>
            <Th className="text-right">Views</Th>
            <Th className="text-right">Sign-ups</Th>
            <Th className="text-right">Sign-up rate</Th>
            <Th>
              <span className="sr-only">Actions</span>
            </Th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.id} className={cx("[&:last-child>td]:border-b-0", r.id === best ? "bg-accent-50/60" : "")}>
              <Td>
                <span className="flex items-center gap-2 font-medium text-zinc-900">
                  {versionLabel(r.id)}
                  {r.id === best ? (
                    <Badge tone="green">
                      <Trophy className="size-3" /> Best rate
                    </Badge>
                  ) : null}
                </span>
              </Td>
              <Td className="text-right tabular-nums">{formatNumber(r.views)}</Td>
              <Td className="text-right tabular-nums">{formatNumber(r.submits)}</Td>
              <Td className={cx("text-right tabular-nums", r.id === best ? "font-semibold text-accent-800" : "")}>{r.views ? formatPercent(r.submit_rate) : "—"}</Td>
              <Td className="text-right">
                {rows.length > 1 ? (
                  <Button size="sm" disabled={!versions.includes(r.id)} title={versions.includes(r.id) ? undefined : "Save your version changes first"} onClick={() => onUse(r.id)}>
                    Use this version
                  </Button>
                ) : null}
              </Td>
            </tr>
          ))}
        </tbody>
      </Table>
      <p className="border-t border-zinc-100 px-4 py-3 text-xs text-zinc-500">
        Small numbers are noisy. A version can lead by chance early on, so wait for a few hundred views per version before picking one. Changing a version's text or image starts its count over when you save.
      </p>
    </Card>
  );
}

import { useState } from "react";
import { useNavigate } from "react-router";
import { BellRing, TrendingDown, Workflow } from "lucide-react";
import { useCreateFlowFromBlueprint } from "../../api";
import { Button, Card, CodeBlock, ColorInput, EmptyState, Field, Input, Page, PageHeader, Skeleton, Toggle } from "../../components/ui";
import { toast } from "../../lib/toast";
import { useCatalogSettings, useCatalogSummary, useUpdateCatalogSettings } from "./api";
import { CatalogTabs } from "./parts";
import type { CatalogSettings, CatalogSettingsPatch } from "./types";

export function AlertsPage() {
  const settings = useCatalogSettings();
  return (
    <Page>
      <PageHeader title="Products" description="Tell people when a product is back in stock or cheaper." />
      <CatalogTabs />
      {settings.isPending ? (
        <Skeleton className="h-64 w-full" />
      ) : settings.error || !settings.data ? (
        <Card>
          <EmptyState title="Couldn't load the settings" description={settings.error?.message} />
        </Card>
      ) : (
        <AlertsForm initial={settings.data} />
      )}
    </Page>
  );
}

const num = (v: string): number | null => (v.trim() === "" ? null : Number(v));

function FlowButton({ blueprint, label }: { blueprint: string; label: string }) {
  const create = useCreateFlowFromBlueprint();
  const navigate = useNavigate();
  return (
    <Button
      size="sm"
      icon={<Workflow className="size-3.5" />}
      loading={create.isPending}
      onClick={() =>
        create.mutate(
          { key: blueprint },
          {
            onSuccess: (f) => {
              toast.success("Draft flow created. Review it, then turn it on.");
              navigate(`/flows/${f.id}`);
            },
          },
        )
      }
    >
      {label}
    </Button>
  );
}

function Row({ title, description, checked, onChange }: { title: string; description: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div>
        <div className="text-sm font-medium text-zinc-900">{title}</div>
        <p className="text-xs text-zinc-500">{description}</p>
      </div>
      <Toggle checked={checked} onChange={onChange} label={title} />
    </div>
  );
}

function AlertsForm({ initial }: { initial: CatalogSettings }) {
  const save = useUpdateCatalogSettings();
  const summary = useCatalogSummary();
  const [s, setS] = useState<CatalogSettings>(initial);
  const [limit, setLimit] = useState(initial.back_in_stock_limit?.toString() ?? "");
  const [percent, setPercent] = useState(initial.price_drop_percent?.toString() ?? "");
  const [amount, setAmount] = useState(initial.price_drop_amount?.toString() ?? "");
  const set = (patch: Partial<CatalogSettings>) => setS({ ...s, ...patch });
  const setWidget = (patch: Partial<CatalogSettings["widget"]>) => setS({ ...s, widget: { ...s.widget, ...patch } });

  const onSave = () => {
    const patch: CatalogSettingsPatch = {
      learn_from_events: s.learn_from_events,
      back_in_stock_enabled: s.back_in_stock_enabled,
      back_in_stock_limit: num(limit),
      price_drop_enabled: s.price_drop_enabled,
      price_drop_percent: num(percent),
      price_drop_amount: num(amount),
      price_drop_lookback_days: s.price_drop_lookback_days,
      price_drop_cooldown_days: s.price_drop_cooldown_days,
      widget: s.widget,
    };
    save.mutate(patch, { onSuccess: () => toast.success("Settings saved") });
  };

  return (
    <div className="space-y-4">
      <Card title={<span className="inline-flex items-center gap-2"><BellRing className="size-4 text-zinc-400" /> Back in stock</span>} actions={<FlowButton blueprint="back_in_stock" label="Create the flow" />}>
        <div className="space-y-4">
          <Row
            title="Take back-in-stock requests"
            description="A “Notify me” button on sold-out product pages. When the product comes back, each person waiting gets a Back in Stock event, oldest request first, and the flow emails them."
            checked={s.back_in_stock_enabled}
            onChange={(back_in_stock_enabled) => set({ back_in_stock_enabled })}
          />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <Field label="Most people told per restock" hint="Leave empty to tell everyone waiting.">
              <Input type="number" min={1} value={limit} onChange={(e) => setLimit(e.target.value)} placeholder="Everyone" />
            </Field>
            <Field label="Waiting now">
              <div className="flex h-9 items-center text-sm text-zinc-900">{summary.data ? summary.data.waiting_for_restock : "—"}</div>
            </Field>
            <Field label="Heading">
              <Input value={s.widget.heading ?? ""} maxLength={100} onChange={(e) => setWidget({ heading: e.target.value })} placeholder="Sold out" />
            </Field>
            <Field label="Button text">
              <Input value={s.widget.button_text ?? ""} maxLength={40} onChange={(e) => setWidget({ button_text: e.target.value })} placeholder="Notify me when it's back" />
            </Field>
            <Field label="Thank-you message" className="sm:col-span-2">
              <Input value={s.widget.success_message ?? ""} maxLength={160} onChange={(e) => setWidget({ success_message: e.target.value })} placeholder="Thanks! We'll email you when it's back." />
            </Field>
            <Field label="Button color" hint="Blank = dark gray">
              <ColorInput value={s.widget.color ?? ""} onChange={(color) => setWidget({ color })} />
            </Field>
            <Field label="Button text color" hint="Blank = white">
              <ColorInput value={s.widget.text_color ?? ""} onChange={(text_color) => setWidget({ text_color })} />
            </Field>
          </div>
          <div>
            <div className="mb-1 text-sm font-medium text-zinc-900">Add it to your product pages</div>
            <p className="mb-2 text-xs text-zinc-500">
              Paste this after the DumbMail snippet. It shows only on sold-out products. Put {"<div data-dumbmail-back-in-stock></div>"} where you want it, or it appears below the add-to-cart button.
            </p>
            <CodeBlock code={initial.widget_snippet} label="HTML" />
          </div>
        </div>
      </Card>

      <Card title={<span className="inline-flex items-center gap-2"><TrendingDown className="size-4 text-zinc-400" /> Price drop</span>} actions={<FlowButton blueprint="price_drop" label="Create the flow" />}>
        <div className="space-y-4">
          <Row
            title="Watch for price drops"
            description="When your feed, a CSV or the API lowers a price, people who viewed or carted the product recently and didn't buy it get a Price Dropped event. Prices seen on your site don't count."
            checked={s.price_drop_enabled}
            onChange={(price_drop_enabled) => set({ price_drop_enabled })}
          />
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <Field label="Smallest drop (%)" hint="With both set, a drop must meet both.">
              <Input type="number" min={1} max={90} value={percent} onChange={(e) => setPercent(e.target.value)} placeholder="10" />
            </Field>
            <Field label="Smallest drop (amount)" hint="In the product's currency.">
              <Input type="number" min={0} step="0.01" value={amount} onChange={(e) => setAmount(e.target.value)} placeholder="Any amount" />
            </Field>
            <Field label="Tell people who looked in the last (days)">
              <Input type="number" min={1} max={90} value={s.price_drop_lookback_days} onChange={(e) => set({ price_drop_lookback_days: Number(e.target.value) })} />
            </Field>
            <Field label="Tell each person about a product at most once every (days)">
              <Input type="number" min={1} max={365} value={s.price_drop_cooldown_days} onChange={(e) => set({ price_drop_cooldown_days: Number(e.target.value) })} />
            </Field>
          </div>
        </div>
      </Card>

      <Card title="Products from your site">
        <Row
          title="Add products people view and buy"
          description="While you have no feed, DumbMail adds products from your site's product views, carts and orders when their page is on your own website. Feed, CSV and API products are never changed by this."
          checked={s.learn_from_events}
          onChange={(learn_from_events) => set({ learn_from_events })}
        />
      </Card>

      <div className="flex justify-end">
        <Button variant="primary" loading={save.isPending} onClick={onSave}>
          Save settings
        </Button>
      </div>
    </div>
  );
}

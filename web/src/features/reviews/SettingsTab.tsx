import { useState } from "react";
import { useNavigate } from "react-router";
import { Plus, Trash2, Workflow } from "lucide-react";
import { useForms } from "../../api/forms";
import { Button, Card, ColorInput, Field, IconButton, InlineError, Input, Notice, Select, Skeleton, TagInput, Toggle } from "../../components/ui";
import { toast } from "../../lib/toast";
import { useCoupons } from "../coupons/api";
import { useCan } from "../team/api";
import { useCreateReviewFlow, useReviewSettings, useUpdateReviewSettings } from "./api";
import type { Question, ReviewSettings, ReviewSettingsPatch } from "./types";

export function SettingsTab() {
  const settings = useReviewSettings();
  const canFlows = useCan("flows.edit");
  if (settings.isPending) return <Skeleton className="h-96 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  return (
    <div className="space-y-4">
      {canFlows ? <FlowCard /> : null}
      <SettingsForm key={JSON.stringify(settings.data)} s={settings.data} />
    </div>
  );
}

function FlowCard() {
  const create = useCreateReviewFlow();
  const navigate = useNavigate();
  const make = (kind: "order" | "stay") =>
    create.mutate(kind, {
      onSuccess: (f) => {
        toast.success(f.note);
        navigate(`/flows/${f.id}`);
      },
      onError: (e) => toast.error(e.message),
    });
  return (
    <Card title="Ask for reviews automatically">
      <div className="space-y-3">
        <p className="max-w-2xl text-sm text-zinc-600">
          Creates a draft flow that emails a review link for each item, then reminds once a week later if they haven't reviewed. It goes only to people subscribed to email.
        </p>
        <div className="flex flex-wrap gap-2">
          <Button icon={<Workflow className="size-3.5" />} loading={create.isPending && create.variables === "order"} onClick={() => make("order")}>
            After an order
          </Button>
          <Button icon={<Workflow className="size-3.5" />} loading={create.isPending && create.variables === "stay"} onClick={() => make("stay")}>
            After a stay
          </Button>
        </div>
        <p className="text-xs text-zinc-500">After an order: 14 days later, or 5 days after delivery if you record deliveries. After a stay: the day after check-out, from your Guesty bookings (Integrations).</p>
      </div>
    </Card>
  );
}

type Draft = Omit<ReviewSettings, "reject_reasons" | "honesty">;

function SettingsForm({ s }: { s: ReviewSettings }) {
  const canManage = useCan("reviews.manage");
  const update = useUpdateReviewSettings();
  const coupons = useCoupons();
  const forms = useForms();
  const [d, setD] = useState<Draft>(() => ({ ...s, questions: s.questions.map((q) => ({ ...q, options: q.options ?? [] })) }));
  const set = (patch: Partial<Draft>) => setD((x) => ({ ...x, ...patch }));
  const setQ = (i: number, patch: Partial<Question>) => set({ questions: d.questions.map((q, j) => (j === i ? { ...q, ...patch } : q)) });
  const save = () => {
    const patch: ReviewSettingsPatch = {
      auto_publish: d.auto_publish,
      auto_publish_min_rating: d.auto_publish_min_rating,
      flag_words: d.flag_words,
      questions: d.questions.map((q) => (q.kind === "choice" ? q : { id: q.id, label: q.label, kind: "text" })),
      public_reviews: d.public_reviews,
      request_days: d.request_days,
      purchase_metrics: d.purchase_metrics,
      thank_you_coupon: d.thank_you_coupon || null,
      subscribe_form_id: d.subscribe_form_id || null,
      widget: Object.fromEntries(Object.entries(d.widget).filter(([, v]) => v !== "" && v !== undefined)),
    };
    update.mutate(patch, { onSuccess: () => toast.success("Settings saved") });
  };
  const signupForms = (forms.data ?? []).filter((f) => f.list_id);
  return (
    <fieldset disabled={!canManage} className="space-y-4">
      <Card title="Publishing">
        <div className="space-y-4">
          <div className="flex items-start gap-3">
            <Toggle checked={d.auto_publish} onChange={(v) => set({ auto_publish: v })} label="Publish verified reviews at once" />
            <div className="text-sm">
              <div className="font-medium text-zinc-900">Publish verified reviews at once</div>
              <p className="text-zinc-500">Only reviews from a real purchase, with nothing flagged. Everything else waits for you.</p>
            </div>
          </div>
          <Field label="With at least" className="max-w-[12rem]">
            <Select value={d.auto_publish_min_rating} onChange={(e) => set({ auto_publish_min_rating: Number(e.target.value) })} disabled={!d.auto_publish}>
              {[1, 2, 3, 4, 5].map((n) => (
                <option key={n} value={n}>
                  {n} star{n === 1 ? "" : "s"}
                </option>
              ))}
            </Select>
          </Field>
          <Notice tone="gray" title="Held reviews still need a fair decision">
            {s.honesty}
          </Notice>
          <Field label="Also count as a purchase or stay" hint="Placed Order, Fulfilled Order, Delivered and your integrations' bookings already do. Only these, recorded by your systems, make a review verified.">
            <TagInput value={d.purchase_metrics} onChange={(v) => set({ purchase_metrics: v })} placeholder="e.g. Reservation Confirmed" aria-label="Purchase metrics" />
          </Field>
          <Field label="Also hold reviews that mention" hint="Profanity, email addresses, phone numbers and links are always held, in the review, the name and the answers.">
            <TagInput value={d.flag_words} onChange={(v) => set({ flag_words: v })} placeholder="Type a word and press Enter" aria-label="Flag words" />
          </Field>
        </div>
      </Card>
      <Card title="The review form">
        <div className="space-y-4">
          <Field label="Links work for" hint="Days after they're sent." className="max-w-[12rem]">
            <Input type="number" min={7} max={365} value={d.request_days} onChange={(e) => set({ request_days: Number(e.target.value) })} />
          </Field>
          <div className="space-y-2">
            <div className="text-xs font-medium text-zinc-700">Your questions (up to 3)</div>
            {d.questions.map((q, i) => (
              <div key={i} className="grid gap-2 rounded-md border border-zinc-200 p-3 sm:grid-cols-[1fr_9rem_auto]">
                <Input value={q.label} onChange={(e) => setQ(i, { label: e.target.value })} placeholder="e.g. How was the fit?" aria-label={`Question ${i + 1}`} />
                <Select value={q.kind} onChange={(e) => setQ(i, { kind: e.target.value as Question["kind"] })} aria-label={`Question ${i + 1} answer type`}>
                  <option value="choice">Choices</option>
                  <option value="text">Short text</option>
                </Select>
                <IconButton label={`Remove question ${i + 1}`} onClick={() => set({ questions: d.questions.filter((_, j) => j !== i) })}>
                  <Trash2 className="size-4" />
                </IconButton>
                {q.kind === "choice" ? (
                  <div className="sm:col-span-3">
                    <TagInput value={q.options ?? []} onChange={(v) => setQ(i, { options: v })} placeholder="Add 2 to 6 choices" aria-label={`Question ${i + 1} choices`} />
                  </div>
                ) : null}
              </div>
            ))}
            {d.questions.length < 3 ? (
              <Button size="sm" icon={<Plus className="size-3.5" />} onClick={() => set({ questions: [...d.questions, { id: `q${Date.now() % 100000}`, label: "", kind: "choice", options: [] }] })}>
                Add a question
              </Button>
            ) : null}
          </div>
          <Field label="Thank-you coupon" hint="Emailed for any review, whatever the rating, to reviewers subscribed to email. The form says so, and those reviews are labelled.">
            <Select value={d.thank_you_coupon ?? ""} onChange={(e) => set({ thank_you_coupon: e.target.value || null })}>
              <option value="">None</option>
              {(coupons.data?.data ?? []).map((c) => (
                <option key={c.id} value={c.name}>
                  {c.name}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Offer to join a list" hint="Shows an unticked box. Ticking it signs them up through this form, with its list's confirmation if it has one.">
            <Select value={d.subscribe_form_id ?? ""} onChange={(e) => set({ subscribe_form_id: e.target.value || null })}>
              <option value="">Don't offer</option>
              {signupForms.map((f) => (
                <option key={f.id} value={f.id}>
                  {f.name}
                </option>
              ))}
            </Select>
          </Field>
          <div className="flex items-start gap-3">
            <Toggle checked={d.public_reviews} onChange={(v) => set({ public_reviews: v })} label="Let visitors write reviews on your site" />
            <div className="text-sm">
              <div className="font-medium text-zinc-900">Let visitors write reviews on your site</div>
              <p className="text-zinc-500">Without a link from an email. They aren't verified, the writer confirms their email first, and you check each one.</p>
            </div>
          </div>
        </div>
      </Card>
      <Card title="Widget look">
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Heading">
            <Input value={d.widget.heading ?? ""} placeholder="Customer reviews" onChange={(e) => set({ widget: { ...d.widget, heading: e.target.value } })} />
          </Field>
          <Field label="Reviews per page">
            <Input type="number" min={3} max={20} value={d.widget.per_page ?? 5} onChange={(e) => set({ widget: { ...d.widget, per_page: Number(e.target.value) } })} />
          </Field>
          <Field label="Stars">
            <ColorInput value={d.widget.star_color ?? ""} placeholder="#f59e0b" onChange={(v) => set({ widget: { ...d.widget, star_color: v } })} aria-label="Star color" />
          </Field>
          <Field label="Buttons and links" hint="Your brand colour when empty.">
            <ColorInput value={d.widget.accent_color ?? ""} onChange={(v) => set({ widget: { ...d.widget, accent_color: v } })} aria-label="Accent color" />
          </Field>
          <Field label="Text">
            <ColorInput value={d.widget.text_color ?? ""} placeholder="#111827" onChange={(v) => set({ widget: { ...d.widget, text_color: v } })} aria-label="Text color" />
          </Field>
        </div>
      </Card>
      {canManage ? (
        <div className="flex items-center gap-3">
          <Button variant="primary" loading={update.isPending} onClick={save}>
            Save settings
          </Button>
          <InlineError message={update.error?.message} />
        </div>
      ) : null}
    </fieldset>
  );
}

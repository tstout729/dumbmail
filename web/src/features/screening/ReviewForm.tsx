import { useState, type FormEvent } from "react";
import { Button, Field, Input, Textarea } from "../../components/ui";
import { useRequestReview } from "./api";
import type { ReviewAnswers } from "./types";

/** The three questions DumbMail asks before an account's first send. */
export function ReviewForm({ initial, submitLabel = "Request review" }: { initial?: ReviewAnswers | null; submitLabel?: string }) {
  const request = useRequestReview();
  const [error, setError] = useState<string | null>(null);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const business = String(d.get("business") ?? "").trim();
    const list_source = String(d.get("list_source") ?? "").trim();
    const monthly_volume = Number(d.get("monthly_volume"));
    if (business.length < 3 || list_source.length < 3) {
      setError("Tell us what your business does and how the people on your list signed up.");
      return;
    }
    if (!Number.isFinite(monthly_volume) || monthly_volume < 0) {
      setError("Enter about how many emails you'll send a month.");
      return;
    }
    setError(null);
    request.mutate({ business, list_source, monthly_volume: Math.round(monthly_volume) });
  };
  return (
    <form onSubmit={onSubmit} className="space-y-4">
      <Field label="What does your business do?">
        <Textarea name="business" rows={2} maxLength={1000} defaultValue={initial?.business ?? ""} placeholder="We make handmade soap in Portland and sell it online." />
      </Field>
      <Field label="How did the people on your list sign up?" hint="For example: a form on your website, at checkout, or in your shop.">
        <Textarea name="list_source" rows={2} maxLength={1000} defaultValue={initial?.list_source ?? ""} placeholder="A newsletter form on our website and a box at checkout." />
      </Field>
      <Field label="About how many emails will you send a month?" className="sm:max-w-60">
        <Input name="monthly_volume" type="number" min={0} max={100000000} step={1} required defaultValue={initial?.monthly_volume ?? ""} placeholder="5000" />
      </Field>
      {error ? <p className="text-xs text-red-600">{error}</p> : null}
      <div className="flex items-center justify-end gap-3">
        {request.isPending ? <span className="text-xs text-zinc-500">Checking your account. This can take up to a minute.</span> : null}
        <Button type="submit" variant="primary" loading={request.isPending}>
          {submitLabel}
        </Button>
      </div>
    </form>
  );
}

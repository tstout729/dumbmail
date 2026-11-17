import type { FormEvent } from "react";
import { Link } from "react-router";
import { ArrowRight } from "lucide-react";
import { useSending, useSession } from "../../api";
import { Button, Card, Field, Input, StatusBadge } from "../../components/ui";
import { formString, useSaveAccount } from "./shared";

export function SendingSettingsPage() {
  const { account } = useSession();
  const { save, saving } = useSaveAccount();
  const sending = useSending();
  const email = sending.data?.email;
  const verified = email?.domains.filter((d) => d.status === "verified").map((d) => d.domain) ?? [];
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const from_email = formString(d, "from_email");
    save({ from_name: formString(d, "from_name"), from_email, reply_to: formString(d, "reply_to"), onboarding: { sender: Boolean(from_email) } });
  };
  return (
    <div className="space-y-4">
      <form onSubmit={onSubmit}>
        <Card title="Default sender">
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <Field label="Sender name">
              <Input name="from_name" defaultValue={account.from_name ?? ""} placeholder={account.name} />
            </Field>
            <Field label="Sender email" hint={verified.length ? `Use an address on ${verified.join(", ")}.` : "Use an address on a domain you verify in Settings → Domains."}>
              <Input name="from_email" type="email" defaultValue={account.from_email ?? ""} placeholder="hello@yourdomain.com" />
            </Field>
            <Field label="Reply-to (optional)" className="sm:col-span-2">
              <Input name="reply_to" type="email" defaultValue={account.reply_to ?? ""} placeholder="support@yourdomain.com" />
            </Field>
          </div>
          <div className="mt-4 flex justify-end">
            <Button type="submit" variant="primary" loading={saving}>
              Save
            </Button>
          </div>
        </Card>
      </form>
      <Link
        to="/settings/domains"
        className="group flex items-center gap-3 rounded-lg border border-zinc-200 bg-white px-4 py-3 shadow-xs hover:border-zinc-300"
      >
        <span className="min-w-0 flex-1">
          <span className="block text-sm font-semibold text-zinc-900">Sending domains</span>
          <span className="block text-xs text-zinc-500">
            {email?.domains.length ? email.domains.map((d) => d.domain).join(", ") : "Verify your domain so your emails land in the inbox."}
          </span>
        </span>
        {email && email.domains.length ? (
          <StatusBadge status={verified.length === email.domains.length ? "verified" : "pending"} label={`${verified.length} of ${email.domains.length} verified`} />
        ) : null}
        <ArrowRight className="size-4 text-zinc-300 group-hover:text-zinc-600" />
      </Link>
    </div>
  );
}

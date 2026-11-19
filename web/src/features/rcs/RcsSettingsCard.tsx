import { useState, type FormEvent } from "react";
import { AlertTriangle, CheckCircle2, Info } from "lucide-react";
import { errorMessage } from "../../api/client";
import { Badge, Button, Card, CopyField, Field, InlineError, Input, Notice, Skeleton, Toggle } from "../../components/ui";
import { formatPrice, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { useCheckRcsAgent, useRcsSettings, useUpdateRcsSettings } from "./api";
import type { RcsSettings } from "./types";

/** Settings → SMS → RCS: the account's RCS agent from Telnyx, its Check, and switching RCS on. */
export function RcsSettingsCard() {
  const settings = useRcsSettings();
  if (settings.isPending) return <Skeleton className="h-48" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  return <RcsSettingsBody s={settings.data} />;
}

function status(s: RcsSettings) {
  if (s.sending) return <Badge tone="green" dot>On</Badge>;
  if (s.needs_check) return <Badge tone="amber" dot>Check again</Badge>;
  if (s.confirmed) return <Badge tone="gray" dot>Off</Badge>;
  return <Badge tone="gray" dot>Not set up</Badge>;
}

function RcsSettingsBody({ s }: { s: RcsSettings }) {
  const canWrite = useCan("settings.write");
  const update = useUpdateRcsSettings();
  const check = useCheckRcsAgent();
  const [agent, setAgent] = useState(s.agent_id ?? "");
  const dirty = agent.trim() !== (s.agent_id ?? "");

  const save = (e: FormEvent) => {
    e.preventDefault();
    update.mutate({ agent_id: agent.trim() || null }, { onSuccess: () => toast.success(agent.trim() ? "Agent saved. Run Check next." : "Agent removed") });
  };
  const runCheck = () =>
    check.mutate(undefined, {
      onSuccess: (r) => (r.confirmed ? toast.success("Telnyx confirmed the agent") : toast.error(r.check_problem ?? "Telnyx didn't confirm the agent")),
      onError: (e) => toast.error(errorMessage(e)),
    });
  const setEnabled = (enabled: boolean) => update.mutate({ enabled }, { onError: (e) => toast.error(errorMessage(e)) });

  return (
    <Card title="RCS" actions={status(s)}>
      <div className="space-y-4">
        <p className="text-sm text-zinc-600">
          RCS sends your texts from your business&apos;s name and logo, with picture cards and buttons, to phones that support it. Everyone else gets the same text as SMS. If an RCS message
          isn&apos;t delivered, it goes once as a normal text.
        </p>
        {!s.available ? (
          <Notice tone="gray" icon={<Info />} title="RCS isn't available for your texts">
            {s.unavailable_reason}
          </Notice>
        ) : (
          <>
            <ol className="list-decimal space-y-1 pl-5 text-sm text-zinc-700">
              <li>
                {s.provider === "own_telnyx"
                  ? "Register an RCS agent in your Telnyx portal. Google reviews your brand and the carriers approve the agent; this can take days to weeks."
                  : "Ask DumbMail support to register an RCS agent for your business. Google reviews your brand and the carriers approve the agent; this can take days to weeks."}
              </li>
              <li>Set the agent&apos;s webhook URL in Telnyx to the address below. It&apos;s private to your account and proves the agent is yours.</li>
              <li>Save the agent id and press Check, then turn RCS on.</li>
            </ol>
            <Field label="Agent webhook URL">
              <CopyField value={s.webhook_url} />
            </Field>
            <form onSubmit={save} className="flex flex-wrap items-end gap-2">
              <Field label="RCS agent id" className="min-w-0 flex-1">
                <Input value={agent} onChange={(e) => setAgent(e.target.value)} placeholder="e.g. acme_agent" disabled={!canWrite} />
              </Field>
              <Button type="submit" disabled={!canWrite || !dirty} loading={update.isPending && dirty}>
                Save
              </Button>
              <Button type="button" variant="secondary" onClick={runCheck} disabled={!canWrite || !s.agent_id || dirty} loading={check.isPending}>
                Check
              </Button>
            </form>
            <InlineError message={update.error && dirty ? errorMessage(update.error) : null} />
            {s.confirmed ? (
              <Notice tone="green" icon={<CheckCircle2 />} title={`Confirmed: ${s.agent_name ?? s.agent_id}`}>
                {s.checked_at ? `Telnyx confirmed it ${formatRelative(s.checked_at)}.` : null}
              </Notice>
            ) : s.check_problem ? (
              <Notice tone="amber" icon={<AlertTriangle />} title="Not confirmed yet">
                {s.check_problem}
              </Notice>
            ) : null}
            {s.needs_check ? (
              <Notice tone="amber" icon={<AlertTriangle />} title="Run Check again">
                Your texting connection changed since the last Check, so texts go as SMS until Telnyx confirms the agent again.
              </Notice>
            ) : null}
            {!s.signing_key_ready ? (
              <Notice tone="amber" icon={<AlertTriangle />} title="Telnyx's webhook signing key is missing">
                RCS replies and opt-outs are only trusted when signed. Add the public key from Telnyx (Keys & Credentials) in the inbox settings before turning RCS on.
              </Notice>
            ) : null}
            <div className="flex items-center justify-between gap-3 border-t border-zinc-100 pt-3">
              <div className="min-w-0">
                <div className="text-sm font-medium text-zinc-800">Send texts as RCS</div>
                <p className="text-xs text-zinc-500">
                  {s.price_per_message > 0
                    ? `${formatPrice(s.price_per_message)} per RCS message, however long. A text that falls back is billed as SMS (${formatPrice(s.price_per_sms_segment)} a segment), never both.`
                    : "Not billed by DumbMail."}
                </p>
              </div>
              <Toggle label="Send texts as RCS" checked={s.enabled} onChange={setEnabled} disabled={!canWrite || update.isPending || (!s.enabled && (!s.confirmed || !s.signing_key_ready))} />
            </div>
          </>
        )}
      </div>
    </Card>
  );
}

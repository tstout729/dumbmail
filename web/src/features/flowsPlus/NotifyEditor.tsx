import { Info } from "lucide-react";
import { Badge, Button, Checkbox, Field, Input, Notice, TagInput, Textarea } from "../../components/ui";
import type { Tone } from "../../components/ui/Layout";
import { toast } from "../../lib/toast";
import { useAskToConfirm, useNotifyAddresses } from "./api";
import type { NotifyAddressStatus, NotifyNode } from "./types";

const MAX = 5;

const statusText: Record<NotifyAddressStatus, [string, Tone]> = {
  team: ["Team member", "green"],
  confirmed: ["Confirmed", "green"],
  pending: ["Waiting to confirm", "amber"],
  waiting: ["Waiting to confirm", "amber"],
  removed: ["Removed", "gray"],
  new: ["Gets a confirm link when you save", "blue"],
  unreachable: ["Mail to it bounced", "red"],
};

/** A notification step: email the team or confirmed addresses when someone reaches it. */
export function NotifyEditor({ node, onChange, readOnly }: { node: NotifyNode; onChange: (n: NotifyNode) => void; readOnly?: boolean }) {
  const to = node.to ?? [];
  const statuses = useNotifyAddresses(to);
  const ask = useAskToConfirm();
  const count = to.length + (node.to_property ? 1 : 0);
  const set = (patch: Partial<NotifyNode>) => onChange({ ...node, ...patch });
  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-4">
      <p className="text-sm text-zinc-600">Email your team (or someone like a property manager) when a person reaches this step. It isn't marketing: it has no unsubscribe link and doesn't count for smart sending.</p>
      <Field label="Step name">
        <Input value={node.name ?? ""} onChange={(e) => set({ name: e.target.value || undefined })} placeholder="Notify the team" />
      </Field>
      <Field label="Send to" hint="Team members' addresses work right away. Anyone else gets a link to confirm first and gets nothing until they do." error={count > MAX ? `At most ${MAX} recipients.` : null}>
        <TagInput value={to} onChange={(v) => set({ to: v.map((e) => e.trim().toLowerCase()).filter(Boolean) })} placeholder="name@example.com" aria-label="Addresses" />
      </Field>
      {to.length > 0 ? (
        <ul className="space-y-1">
          {to.map((e) => {
            const st = statuses.data?.find((s) => s.email === e)?.status;
            const [label, tone] = st ? statusText[st] : ["Checking…", "gray" as Tone];
            return (
              <li key={e} className="flex flex-wrap items-center justify-between gap-2 text-sm">
                <span className="min-w-0 truncate text-zinc-800">{e}</span>
                <span className="flex items-center gap-2">
                  <Badge tone={tone}>{label}</Badge>
                  {st === "pending" || st === "removed" || st === "waiting" ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      loading={ask.isPending && ask.variables === e}
                      onClick={() =>
                        ask.mutate(e, {
                          onSuccess: (r) => (r.status === "pending" ? toast.success(`Sent a confirm link to ${e}`) : toast.success(`A link went to ${e} within the last hour; try again later`)),
                        })
                      }
                    >
                      Send link again
                    </Button>
                  ) : null}
                </span>
              </li>
            );
          })}
        </ul>
      ) : null}
      <Field label="Or the address in a profile property" hint="For example manager_email. It must also be a team member's or a confirmed address.">
        <Input value={node.to_property ?? ""} onChange={(e) => set({ to_property: e.target.value.trim() || undefined })} placeholder="manager_email" />
      </Field>
      <Field label="Subject">
        <Input value={node.subject} onChange={(e) => set({ subject: e.target.value })} placeholder="New booking: {{ person.email }}" maxLength={200} />
      </Field>
      <Field label="Message" hint="Plain text. Use {{ person.first_name }}, {{ person.email }} or {{ event.properties.name }}. Coupon codes can't be used here.">
        <Textarea rows={6} value={node.body} onChange={(e) => set({ body: e.target.value })} maxLength={5000} placeholder={"{{ person.first_name }} just booked.\n\nCheck-in: {{ event.properties.check_in }}"} />
      </Field>
      <Checkbox
        label="Also for events sent from my website"
        description="Off by default: anyone can send website events, so a broken or abused form could flood your inbox."
        checked={!!node.allow_browser_events}
        onChange={(e) => set({ allow_browser_events: e.target.checked || undefined })}
      />
      <Notice tone="gray" icon={<Info />} title="Limits">
        Up to {MAX} recipients. A flow sends at most 200 notifications a day (change it in the flow's settings, up to 2,000). Each one is billed like any email.
      </Notice>
    </fieldset>
  );
}

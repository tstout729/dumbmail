import { Pencil } from "lucide-react";
import type { EmailContent, FlowNode, SmsContent } from "../../api";
import { starterEmail } from "../../components/email/blocks";
import { EmailPreview } from "../../components/email/EmailPreview";
import { SmsEditor } from "../../components/email/SmsEditor";
import { Button, Checkbox, Field, Input, Notice, Segmented } from "../../components/ui";
import type { ReviewRequestNode } from "./types";

export function isEmailContent(c: EmailContent | SmsContent): c is EmailContent {
  return "subject" in c || "blocks" in c || "html" in c;
}

export function reviewEmail(brandColor?: string, reminder = false): EmailContent {
  return {
    ...starterEmail(brandColor),
    subject: reminder ? "A quick reminder: how was it?" : "How did we do, {{ first_name|default('friend') }}?",
    preheader: "Your review takes a minute",
    blocks: [
      { type: "heading", text: reminder ? "Still happy to hear from you" : "How was your order?", level: 1, align: "left" },
      { type: "text", html: "<p>Tap the stars to tell us what you think. Your review helps other people choose.</p>", align: "left" },
      { type: "review_request", button_text: "Write a review", show_stars: true },
    ],
  };
}

export function newReviewRequestNode(id: string, brandColor?: string): ReviewRequestNode {
  return { id, type: "review_request", name: "Ask for a review", channel: "email", reminder: false, content: reviewEmail(brandColor), next: null };
}

export function reviewStepSummary(node: ReviewRequestNode): { text: string; warning?: string } {
  if (isEmailContent(node.content)) return node.content.subject ? { text: `${node.reminder ? "Reminder · " : ""}${node.content.subject}` } : { text: "No subject", warning: "Add a subject line" };
  return node.content.body ? { text: `${node.reminder ? "Reminder · " : ""}${node.content.body}` } : { text: "Empty message", warning: "Write a message" };
}

export function ReviewStepEditor({ node, onChange, onEditEmail, readOnly }: { node: ReviewRequestNode; onChange: (n: FlowNode) => void; onEditEmail: () => void; readOnly?: boolean }) {
  const email = isEmailContent(node.content) ? node.content : null;
  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-600">Emails or texts each person a review link for every item in the event that started the flow. Only people subscribed on that channel get it, and nobody is asked twice about the same order line or about something they reviewed this year.</p>
      <fieldset disabled={readOnly} className="min-w-0 space-y-3">
        <Field label="Step name">
          <Input value={node.name ?? ""} onChange={(e) => onChange({ ...node, name: e.target.value || undefined })} placeholder="Ask for a review" />
        </Field>
        <Segmented
          value={node.channel}
          onChange={(channel) => onChange({ ...node, channel, content: channel === "sms" ? { body: "How was it? Tell us in a quick review: {% review_link %}" } : reviewEmail(undefined, node.reminder) })}
          options={[
            { value: "email", label: "Email" },
            { value: "sms", label: "Text" },
          ]}
        />
        <Checkbox
          checked={node.reminder ?? false}
          onChange={(e) => onChange({ ...node, reminder: e.target.checked })}
          label="This is a reminder"
          description="Sends again only the links from earlier in this flow that haven't been used, once. Put a delay before it."
        />
      </fieldset>
      {email ? (
        <>
          <div className="rounded-lg border border-zinc-200 p-3">
            <div className="text-xs text-zinc-500">Subject</div>
            <div className="text-sm font-medium text-zinc-900">{email.subject || "No subject yet"}</div>
          </div>
          {!(email.blocks ?? []).some((b) => b.type === "review_request") && !JSON.stringify(email).includes("review_link") ? (
            <Notice tone="amber" title="Add a Review request block">Without it, or a {"{% review_link %}"} link, the email doesn't carry the review links.</Notice>
          ) : null}
          <Button variant="primary" icon={<Pencil className="size-3.5" />} onClick={onEditEmail} className="w-full">
            {readOnly ? "View email" : "Edit email"}
          </Button>
          <EmailPreview content={email} className="h-[520px]" />
        </>
      ) : (
        <SmsEditor value={node.content as SmsContent} onChange={(content) => onChange({ ...node, content })} readOnly={readOnly} templateName={node.name} />
      )}
    </div>
  );
}

import { MailWarning } from "lucide-react";
import { useSession } from "../../api";
import { Notice } from "../ui";

/** Shown on every page while the account's email sending is paused for high bounces or complaints. */
export function SendingPausedBanner() {
  const { account, support_email } = useSession();
  if (!account.sending_paused_at) return null;
  const contact = support_email ? (
    <a className="font-medium underline" href={`mailto:${support_email}?subject=${encodeURIComponent(`Email sending paused: ${account.name}`)}`}>
      {support_email}
    </a>
  ) : (
    "support"
  );
  return (
    <div className="border-b border-red-100 bg-white px-4 py-3 sm:px-6 lg:px-8">
      <Notice tone="red" icon={<MailWarning />} title="Email sending is paused">
        Recent emails from this account had too many bounces or spam complaints
        {account.sending_paused_reason ? ` (${account.sending_paused_reason})` : ""}. Every DumbMail account shares one sending
        service, so we stopped your email campaigns, flow emails and test emails to keep delivery working for everyone. Texts still
        send. Remove addresses that bounced or never signed up (bought, rented or very old lists), then contact {contact} to turn
        email back on.
      </Notice>
    </div>
  );
}

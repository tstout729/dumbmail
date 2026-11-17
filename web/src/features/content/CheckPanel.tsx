import { AlertTriangle, CheckCircle2, ClipboardCheck, Info, XCircle } from "lucide-react";
import type { Audience, EmailContent } from "../../api/types";
import { Button, cx } from "../../components/ui";
import { useCheckEmail } from "./api";
import type { CheckFinding, CheckResult } from "./types";

type Props = {
  content: EmailContent;
  campaignId?: string;
  audience?: Audience;
  flowId?: string;
  nodeId?: string;
  className?: string;
};

function Icon({ f }: { f: CheckFinding }) {
  if (f.severity === "error") return <XCircle className="mt-0.5 size-4 shrink-0 text-red-500" />;
  if (f.severity === "warning") return <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-500" />;
  return <Info className="mt-0.5 size-4 shrink-0 text-zinc-400" />;
}

function Summary({ r }: { r: CheckResult }) {
  const kb = r.size_bytes ? `${Math.round(r.size_bytes / 1024)} KB` : null;
  const checked = r.links.length;
  return (
    <p className="text-xs text-zinc-500">
      {[
        r.errors ? `${r.errors} to fix before sending` : null,
        r.warnings ? `${r.warnings} worth fixing` : null,
        !r.errors && !r.warnings ? "Nothing to fix" : null,
        kb,
        checked ? `${checked} links and images checked` : null,
        r.recipients !== null ? `${r.recipients.toLocaleString()} recipients` : null,
      ]
        .filter(Boolean)
        .join(" · ")}
    </p>
  );
}

/** Run the checks before sending and list what to fix. */
export function CheckPanel({ content, campaignId, audience, flowId, nodeId, className }: Props) {
  const check = useCheckEmail();
  const r = check.data;
  const run = () => check.mutate({ content, campaign_id: campaignId, audience, flow_id: flowId, node_id: nodeId });
  return (
    <div className={cx("rounded-lg border border-zinc-200 bg-white p-4", className)}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <div className="text-sm font-medium text-zinc-900">Check before sending</div>
          <p className="text-xs text-zinc-500">Links, images, subject, size, personalization, audience and spam signals.</p>
        </div>
        <Button size="sm" variant={r ? "ghost" : "secondary"} icon={<ClipboardCheck className="size-3.5" />} loading={check.isPending} onClick={run}>
          {r ? "Check again" : "Run checks"}
        </Button>
      </div>
      {r ? (
        <div className="mt-3 space-y-2">
          <Summary r={r} />
          {r.findings.length === 0 ? (
            <div className="flex items-center gap-2 text-sm text-zinc-700">
              <CheckCircle2 className="size-4 text-accent-600" /> Looks good.
            </div>
          ) : (
            <ul className="space-y-2">
              {r.findings.map((f, i) => (
                <li key={`${f.id}-${i}`} className="flex items-start gap-2 text-sm">
                  <Icon f={f} />
                  <span className="min-w-0">
                    <span className="block text-zinc-800">
                      {f.title}
                      {f.blocks_sending ? <span className="ml-1.5 rounded bg-red-50 px-1.5 py-0.5 text-[11px] font-medium text-red-700">Blocks sending</span> : null}
                    </span>
                    <span className="block whitespace-pre-line break-words text-xs text-zinc-500">{f.detail}</span>
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      ) : null}
    </div>
  );
}

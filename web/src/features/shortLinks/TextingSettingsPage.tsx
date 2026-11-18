import { Link } from "react-router";
import { Card, InlineError, Skeleton, Toggle } from "../../components/ui";
import { useShortLinkSettings, useUpdateShortLinkSettings } from "./api";

/** Settings → Texting: short links in text messages. */
export function TextingSettingsPage() {
  const settings = useShortLinkSettings();
  const update = useUpdateShortLinkSettings();
  if (settings.isPending) return <Skeleton className="h-40 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  const s = settings.data;
  return (
    <div className="space-y-4">
      <Card title="Short links">
        <div className="flex items-start justify-between gap-4">
          <p className="text-sm text-zinc-600">
            Texts are billed by length, so DumbMail turns each link you write into a short link of {s.link_length} characters, whatever it points to. A long
            link no longer costs you an extra segment.
          </p>
          <Toggle checked={s.enabled} onChange={(enabled) => update.mutate({ enabled })} label="Shorten links in texts" disabled={update.isPending} />
        </div>
        <div className="mt-3 rounded-md bg-zinc-50 px-3 py-2">
          <div className="text-xs text-zinc-500">{s.enabled ? "Every link becomes" : "Off. Links keep their long tracked form, which carries the whole address."}</div>
          {s.enabled ? <code className="mt-0.5 block break-all font-mono text-xs text-zinc-800">{s.sample}</code> : null}
        </div>
        <ul className="mt-3 list-disc space-y-1 pl-5 text-xs text-zinc-500">
          <li>Clicks are counted the same way, and the link opens the page you wrote.</li>
          <li>
            Links get your{" "}
            <Link to="/settings/tracking" className="font-medium text-zinc-700 underline">
              link tracking (UTM)
            </Link>{" "}
            tags when they're shortened, at no extra length.
          </li>
          <li>Links made from a variable, like a product link, and DumbMail's own links keep the long form.</li>
          <li>A campaign keeps the setting it started sending with. Links already sent keep working.</li>
        </ul>
        {!s.ready && s.enabled ? <p className="mt-2 text-xs text-amber-700">Getting ready after an update. Texts use long links for a few minutes.</p> : null}
        <InlineError message={update.error?.message} />
      </Card>
    </div>
  );
}

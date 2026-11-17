import { useState, type FormEvent } from "react";
import { Button, Card, Field, InlineError, Input, Segmented, Skeleton } from "../../components/ui";
import { useSendSettings, useUpdateSendSettings } from "./api";
import { clockLabel } from "./options";
import type { SendSettings, SmsZoneConflict } from "./types";

/** Settings → Send times: SMS quiet hours, and how each person's time zone is found. */
export function SendSettingsPage() {
  const settings = useSendSettings();
  return (
    <div className="space-y-4">
      {settings.isPending ? (
        <Skeleton className="h-40 w-full" />
      ) : settings.error ? (
        <InlineError message={settings.error.message} />
      ) : (
        <>
          <QuietHours key={`${settings.data.sms_window_start}-${settings.data.sms_window_end}`} settings={settings.data} />
          <ZoneConflict settings={settings.data} />
        </>
      )}
      <Card title="Each person's time zone">
        <p className="text-sm text-zinc-600">Local-time sends, best-time sends and text message hours use each person's own time zone. DumbMail finds it from, in order:</p>
        <ol className="mt-2 list-decimal space-y-1 pl-5 text-sm text-zinc-600">
          <li>
            Their <code className="rounded bg-zinc-100 px-1 text-xs">timezone</code> property (sign-up forms fill it from the browser).
          </li>
          <li>Their US state, Canadian province or Australian state.</li>
          <li>Their US ZIP code.</li>
          <li>Their country, when it has one time zone.</li>
          <li>Their phone number's country, or its US or Canadian area code.</li>
          <li>Your account's time zone, when nothing else tells us.</li>
        </ol>
      </Card>
    </div>
  );
}

function QuietHours({ settings }: { settings: SendSettings }) {
  const update = useUpdateSendSettings();
  const [start, setStart] = useState(settings.sms_window_start);
  const [end, setEnd] = useState(settings.sms_window_end);
  const legal = settings.legal_sms_window;
  const tooWide = start < legal.start || end > legal.end;
  const tooShort = !tooWide && end <= start;
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    update.mutate({ sms_window_start: start, sms_window_end: end });
  };
  return (
    <form onSubmit={onSubmit}>
      <Card title="Text message hours">
        <p className="mb-4 text-sm text-zinc-600">
          Texts from campaigns and flows only go out during these hours on the clock of the recipient's phone number. A text outside them waits until they
          open. You can narrow the window but not go past {clockLabel(legal.start)} to {clockLabel(legal.end)}. When someone's saved time zone differs from
          their phone number's, a text waits until it's within these hours in both, if the two ever overlap.
        </p>
        <div className="flex flex-wrap items-end gap-3">
          <Field label="From">
            <Input type="time" value={start} onChange={(e) => setStart(e.target.value)} className="w-32" required />
          </Field>
          <Field label="Until">
            <Input type="time" value={end} onChange={(e) => setEnd(e.target.value)} className="w-32" required />
          </Field>
          <Button type="submit" variant="primary" loading={update.isPending} disabled={tooWide || tooShort || (start === settings.sms_window_start && end === settings.sms_window_end)}>
            Save
          </Button>
          <Button
            type="button"
            variant="ghost"
            onClick={() => {
              setStart(settings.default_sms_window.start);
              setEnd(settings.default_sms_window.end);
            }}
          >
            Use {clockLabel(settings.default_sms_window.start)} to {clockLabel(settings.default_sms_window.end)}
          </Button>
        </div>
        {tooWide ? <p className="mt-2 text-xs text-red-600">Keep it between {clockLabel(legal.start)} and {clockLabel(legal.end)}.</p> : null}
        {tooShort ? <p className="mt-2 text-xs text-red-600">The end has to come after the start.</p> : null}
        {update.error ? <p className="mt-2 text-xs text-red-600">{update.error.message}</p> : null}
      </Card>
    </form>
  );
}

const conflictOptions: { value: SmsZoneConflict; label: string }[] = [
  { value: "phone", label: "Use the phone number" },
  { value: "skip", label: "Don't text them" },
];

/** What happens when someone's saved time zone and phone number never share texting hours. */
function ZoneConflict({ settings }: { settings: SendSettings }) {
  const update = useUpdateSendSettings();
  return (
    <Card title="When someone's time zone and phone number disagree">
      <p className="mb-3 text-sm text-zinc-600">
        Sometimes a saved time zone and a phone number are so far apart that their texting hours never overlap, like a New York number with a Manila time
        zone. The phone number's zone is the one carriers go by. Either way, the message on their profile says what happened.
      </p>
      <Segmented
        value={settings.sms_zone_conflict}
        onChange={(v) => update.mutate({ sms_zone_conflict: v })}
        options={conflictOptions}
      />
      <p className="mt-2 text-xs text-zinc-500">
        {settings.sms_zone_conflict === "phone"
          ? "They're texted during these hours on their phone number's clock."
          : "They're not texted; the text is skipped with the reason \"time zones disagree\"."}
      </p>
      {update.error ? <p className="mt-2 text-xs text-red-600">{update.error.message}</p> : null}
    </Card>
  );
}

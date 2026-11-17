import { useState, type FormEvent } from "react";
import { AlertTriangle } from "lucide-react";
import { errorMessage } from "../../api";
import { Button, CopyButton, Field, InlineError, Input, Skeleton } from "../../components/ui";
import { useEnableTwoFactor, useSetupTwoFactor } from "./api";

/** Scan a QR code, confirm with a code, then save the recovery codes. Used in Security and at sign-in. */
export function TwoFactorSetupFlow({ onDone }: { onDone: () => void }) {
  const setup = useSetupTwoFactor();
  const enable = useEnableTwoFactor();
  const [codes, setCodes] = useState<string[] | null>(null);
  const started = setup.data;

  if (codes) return <RecoveryCodes codes={codes} onDone={onDone} />;
  if (!started) {
    return (
      <div className="space-y-3 text-sm text-zinc-600">
        <p>You'll need an authenticator app on your phone, like 1Password, Google Authenticator or Authy.</p>
        <InlineError message={setup.error ? errorMessage(setup.error) : null} />
        <Button variant="primary" loading={setup.isPending} onClick={() => setup.mutate()}>
          Start setup
        </Button>
      </div>
    );
  }
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const code = String(new FormData(e.currentTarget).get("code"));
    enable.mutate(code, { onSuccess: (r) => setCodes(r.recovery_codes) });
  };
  return (
    <form onSubmit={onSubmit} className="space-y-4 text-sm text-zinc-600">
      <p>1. Scan this code with your authenticator app.</p>
      <div className="flex flex-wrap items-center gap-4">
        {started.qr_code ? <img src={started.qr_code} alt="QR code for your authenticator app" className="size-44 rounded-md border border-zinc-200 bg-white" /> : <Skeleton className="size-44" />}
        <div className="min-w-0 flex-1 space-y-1">
          <div className="text-xs text-zinc-500">Can't scan? Enter this key instead:</div>
          <div className="flex items-center gap-2">
            <code className="break-all rounded bg-zinc-100 px-1.5 py-0.5 font-mono text-xs text-zinc-800">{started.secret}</code>
            <CopyButton text={started.secret} />
          </div>
        </div>
      </div>
      <Field label="2. Enter the 6-digit code it shows">
        <Input name="code" inputMode="numeric" autoComplete="one-time-code" pattern="[0-9 ]{6,7}" required className="w-36" autoFocus />
      </Field>
      <InlineError message={enable.error ? errorMessage(enable.error) : null} />
      <Button type="submit" variant="primary" loading={enable.isPending}>
        Turn on two-factor login
      </Button>
    </form>
  );
}

export function RecoveryCodes({ codes, onDone }: { codes: string[]; onDone: () => void }) {
  return (
    <div className="space-y-3 text-sm">
      <div className="flex items-start gap-2 rounded-md bg-amber-50 px-3 py-2 text-amber-800">
        <AlertTriangle className="mt-0.5 size-4 shrink-0" />
        Save these recovery codes somewhere safe. Each one signs you in once if you lose your phone. You won't see them again.
      </div>
      <ul className="grid grid-cols-2 gap-1.5 rounded-md border border-zinc-200 bg-zinc-50 p-3 font-mono text-sm text-zinc-800">
        {codes.map((c) => (
          <li key={c}>{c}</li>
        ))}
      </ul>
      <div className="flex flex-wrap gap-2">
        <CopyButton text={codes.join("\n")} label="Copy codes" />
        <Button variant="primary" onClick={onDone}>
          I've saved them
        </Button>
      </div>
    </div>
  );
}

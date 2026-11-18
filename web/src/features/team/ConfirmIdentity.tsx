import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { useSearchParams } from "react-router";
import { errorMessage, useMe } from "../../api";
import { toast } from "../../lib/toast";
import { Button, Field, InlineError, Input, Modal } from "../../components/ui";
import { useReauth } from "./api";
import type { TeamSessionInfo } from "./types";

type Props = {
  title: string;
  body: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  /** Runs after the password or code checks out. */
  onConfirmed: () => Promise<unknown>;
  onClose: () => void;
};

type Mode = "password" | "code" | "google";

/** Send the browser to Google to sign in again; it comes back to this page, confirmed. */
export function confirmWithGoogle() {
  const here = `${window.location.pathname}${window.location.search}`;
  window.location.assign(`/api/auth/google?reauth=1&next=${encodeURIComponent(here)}`);
}

const googleErrors: Record<string, string> = {
  google_cancelled: "Google sign-in was cancelled, so nothing was confirmed.",
  google_other_account: "That's a different Google account from the one your login uses.",
  google_not_fresh: "Google didn't ask you to sign in again, so it couldn't confirm it's you. Use your password or a two-factor code instead.",
  google_session: "This browser's session changed while you were at Google. Try again.",
  google_unavailable: "Confirming with Google works only for logins that sign in with Google.",
  google_failed: "Google couldn't confirm it's you. Try again.",
};

/** After coming back from Google: say whether it confirmed it's you, then tidy the address. */
export function useGoogleConfirmResult() {
  const [params, setParams] = useSearchParams();
  const confirmed = params.get("confirmed");
  const failed = params.get("confirm_error");
  // External system: the address bar, which Google's return wrote to.
  useEffect(() => {
    if (!confirmed && !failed) return;
    if (confirmed === "google") toast.success("Confirmed with Google. For the next 10 minutes you can make ownership changes.");
    else if (failed) toast.error(googleErrors[failed] ?? googleErrors.google_failed);
    const next = new URLSearchParams(params);
    next.delete("confirmed");
    next.delete("confirm_error");
    setParams(next, { replace: true });
  }, [confirmed, failed, params, setParams]);
}

/**
 * Ask for the password, a two-factor code, or a fresh Google sign-in before an ownership change.
 * The server only allows making, removing or demoting owners within a few minutes of this.
 */
export function ConfirmIdentityModal({ title, body, confirmLabel, danger, onConfirmed, onClose }: Props) {
  const me = useMe();
  const methods = (me.data as (typeof me.data & Partial<TeamSessionInfo>) | undefined)?.login_methods ?? { password: true, google: false };
  const reauth = useReauth();
  const modes: Mode[] = [...(methods.password ? (["password"] as const) : []), ...(methods.google ? (["google"] as const) : []), "code"];
  const [mode, setMode] = useState<Mode>(modes[0]);
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const onSubmit = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    if (mode === "google") {
      confirmWithGoogle();
      return;
    }
    setError(null);
    setBusy(true);
    try {
      await reauth.mutateAsync(mode === "password" ? { password: value } : { code: value });
      await onConfirmed();
      onClose();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const switchTo = (m: Mode) => {
    setMode(m);
    setValue("");
    setError(null);
  };
  const otherLabel: Record<Mode, string> = {
    password: "Use your password instead",
    google: "Confirm with Google instead",
    code: "Use a two-factor code instead",
  };

  return (
    <Modal title={title} onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <div className="text-sm text-zinc-600">{body}</div>
        {mode === "password" ? (
          <Field label="Your password" hint="To confirm it's you.">
            <Input type="password" autoComplete="current-password" required autoFocus value={value} onChange={(e) => setValue(e.target.value)} />
          </Field>
        ) : mode === "code" ? (
          <Field label="Code from your authenticator app" hint="Or one of your recovery codes.">
            <Input autoComplete="one-time-code" required autoFocus value={value} onChange={(e) => setValue(e.target.value)} />
          </Field>
        ) : (
          <p className="text-sm text-zinc-600">
            Google asks you to sign in again, then brings you back to this page. Then do this again within 10 minutes.
          </p>
        )}
        <div className="flex flex-col items-start gap-1">
          {modes
            .filter((m) => m !== mode)
            .map((m) => (
              <button key={m} type="button" className="text-sm font-medium text-accent-700 hover:underline" onClick={() => switchTo(m)}>
                {otherLabel[m]}
              </button>
            ))}
        </div>
        <InlineError message={error} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant={danger && mode !== "google" ? "danger" : "primary"} loading={busy}>
            {mode === "google" ? "Continue with Google" : confirmLabel}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

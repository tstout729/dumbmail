import { useState, type FormEvent, type ReactNode } from "react";
import { errorMessage } from "../../api";
import { Button, Field, InlineError, Input, Modal } from "../../components/ui";
import { useReauth } from "./api";

type Props = {
  title: string;
  body: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  /** Runs after the password or code checks out. */
  onConfirmed: () => Promise<unknown>;
  onClose: () => void;
};

/**
 * Ask for the password (or a two-factor code) before an ownership change. The server only allows
 * making, removing or demoting owners within a few minutes of this.
 */
export function ConfirmIdentityModal({ title, body, confirmLabel, danger, onConfirmed, onClose }: Props) {
  const reauth = useReauth();
  const [mode, setMode] = useState<"password" | "code">("password");
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const onSubmit = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
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

  return (
    <Modal title={title} onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <div className="text-sm text-zinc-600">{body}</div>
        {mode === "password" ? (
          <Field label="Your password" hint="To confirm it's you.">
            <Input type="password" autoComplete="current-password" required autoFocus value={value} onChange={(e) => setValue(e.target.value)} />
          </Field>
        ) : (
          <Field label="Code from your authenticator app" hint="Or one of your recovery codes.">
            <Input autoComplete="one-time-code" required autoFocus value={value} onChange={(e) => setValue(e.target.value)} />
          </Field>
        )}
        <button
          type="button"
          className="text-sm font-medium text-accent-700 hover:underline"
          onClick={() => {
            setMode(mode === "password" ? "code" : "password");
            setValue("");
            setError(null);
          }}
        >
          {mode === "password" ? "Use a two-factor code instead" : "Use your password instead"}
        </button>
        <InlineError message={error} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant={danger ? "danger" : "primary"} loading={busy}>
            {confirmLabel}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

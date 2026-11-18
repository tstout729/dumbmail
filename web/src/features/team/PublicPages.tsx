import { useState, type FormEvent } from "react";
import { Link, Navigate, useParams, useSearchParams } from "react-router";
import { ApiError, errorMessage, useLogout, useMe } from "../../api";
import { Button, ButtonLink, ConfirmDialog, Field, InlineError, Input, Skeleton } from "../../components/ui";
import { AuthLayout } from "../../pages/auth/AuthLayout";
import { GoogleSignIn } from "../../pages/auth/GoogleButton";
import { useAcceptInvite, useConfirmEmail, useForgotPassword, useGoogleLink, useLinkWithPassword, usePublicInvite, useResetPassword, useStartFresh } from "./api";
import { ConfirmIdentityModal, useGoogleConfirmResult } from "./ConfirmIdentity";
import { roleLabels } from "./types";

const backToLogin = (
  <Link to="/login" className="font-medium text-accent-700 hover:underline">
    Back to log in
  </Link>
);

/** `/invite/:token`: join an account from an invite email. */
export function InvitePage() {
  const { token = "" } = useParams();
  const invite = usePublicInvite(token);
  const me = useMe();
  const accept = useAcceptInvite();
  const logout = useLogout();
  const [confirming, setConfirming] = useState(false);
  useGoogleConfirmResult();
  const next = `/invite/${token}`;

  if (invite.isPending || me.isPending) {
    return (
      <AuthLayout title="Join a team" footer={backToLogin}>
        <Skeleton className="h-24 w-full" />
      </AuthLayout>
    );
  }
  if (invite.error) {
    const gone = invite.error instanceof ApiError && invite.error.status === 404;
    return (
      <AuthLayout title="This invite doesn't work" footer={backToLogin}>
        <p className="text-sm text-zinc-600">{gone ? "The link expired, was used already, or was cancelled. Ask whoever invited you for a new one." : errorMessage(invite.error)}</p>
      </AuthLayout>
    );
  }
  const inv = invite.data;
  const subtitle = `${inv.invited_by ?? "Someone"} invited ${inv.email} to join as ${roleLabels[inv.role]}.`;
  const signedIn = me.data ? inv.signed_in_as : null;

  if (signedIn) {
    const same = signedIn.toLowerCase() === inv.email.toLowerCase();
    return (
      <AuthLayout title={`Join ${inv.account_name}`} subtitle={subtitle} footer={backToLogin}>
        {same ? (
          <div className="space-y-3">
            <InlineError message={accept.error ? errorMessage(accept.error) : null} />
            <Button
              variant="primary"
              className="w-full"
              loading={accept.isPending}
              onClick={() => (inv.confirm_identity_first ? setConfirming(true) : accept.mutate({ token }))}
            >
              Accept and open {inv.account_name}
            </Button>
            {confirming ? (
              <ConfirmIdentityModal
                title="Confirm it's you"
                body={<p>This invite makes you an owner of {inv.account_name}. Confirm it's you first.</p>}
                confirmLabel="Accept"
                onConfirmed={() => accept.mutateAsync({ token })}
                onClose={() => setConfirming(false)}
              />
            ) : null}
          </div>
        ) : (
          <div className="space-y-3 text-sm text-zinc-600">
            <p>
              You're signed in as {signedIn}. This invite is for {inv.email}. Log out, then open the invite link again.
            </p>
            <Button className="w-full" onClick={() => logout.mutate()}>
              Log out
            </Button>
          </div>
        )}
      </AuthLayout>
    );
  }

  if (inv.has_login) {
    return (
      <AuthLayout title={`Join ${inv.account_name}`} subtitle={subtitle} footer={backToLogin}>
        <p className="mb-4 text-sm text-zinc-600">{inv.email} already has a DumbMail login. Log in with it to accept.</p>
        <ButtonLink to={`/login?next=${encodeURIComponent(next)}`} variant="primary" className="w-full">
          Log in to accept
        </ButtonLink>
      </AuthLayout>
    );
  }

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    accept.mutate({ token, name: String(d.get("name")).trim() || undefined, password: String(d.get("password")) });
  };
  return (
    <AuthLayout title={`Join ${inv.account_name}`} subtitle={subtitle} footer={backToLogin}>
      <GoogleSignIn next={next} />
      <form onSubmit={onSubmit} className="mt-4 space-y-3.5">
        <Field label="Email">
          <Input value={inv.email} disabled readOnly />
        </Field>
        <Field label="Your name">
          <Input name="name" autoComplete="name" autoFocus />
        </Field>
        <Field label="Password" hint="At least 8 characters.">
          <Input name="password" type="password" autoComplete="new-password" minLength={8} required />
        </Field>
        <InlineError message={accept.error ? errorMessage(accept.error) : null} />
        <Button type="submit" variant="primary" className="w-full" loading={accept.isPending}>
          Join {inv.account_name}
        </Button>
      </form>
    </AuthLayout>
  );
}

/** `/forgot-password` */
export function ForgotPasswordPage() {
  const forgot = useForgotPassword();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    forgot.mutate(String(new FormData(e.currentTarget).get("email")).trim());
  };
  return (
    <AuthLayout title="Forgot your password?" subtitle="We'll email you a link to set a new one." footer={backToLogin}>
      {forgot.isSuccess ? (
        <p className="text-sm text-zinc-600">{forgot.data.message} The link works for one hour.</p>
      ) : (
        <form onSubmit={onSubmit} className="space-y-3.5">
          <Field label="Email">
            <Input name="email" type="email" autoComplete="email" required autoFocus />
          </Field>
          <InlineError message={forgot.error ? errorMessage(forgot.error) : null} />
          <Button type="submit" variant="primary" className="w-full" loading={forgot.isPending}>
            Send link
          </Button>
        </form>
      )}
    </AuthLayout>
  );
}

/** `/reset-password?token=…` from the reset email. */
export function ResetPasswordPage() {
  const [params] = useSearchParams();
  const token = params.get("token") ?? "";
  const reset = useResetPassword();
  const [mismatch, setMismatch] = useState(false);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const password = String(d.get("password"));
    setMismatch(password !== String(d.get("again")));
    if (password === String(d.get("again"))) reset.mutate({ token, password });
  };
  if (reset.isSuccess) {
    return (
      <AuthLayout title="Password changed" subtitle="You were signed out everywhere. Log in with your new password." footer={backToLogin}>
        <ButtonLink to="/login" variant="primary" className="w-full">
          Log in
        </ButtonLink>
      </AuthLayout>
    );
  }
  return (
    <AuthLayout title="Set a new password" footer={backToLogin}>
      <form onSubmit={onSubmit} className="space-y-3.5">
        <Field label="New password" hint="At least 8 characters.">
          <Input name="password" type="password" autoComplete="new-password" minLength={8} required autoFocus />
        </Field>
        <Field label="New password again">
          <Input name="again" type="password" autoComplete="new-password" minLength={8} required />
        </Field>
        <InlineError message={mismatch ? "The two passwords don't match." : reset.error ? errorMessage(reset.error) : null} />
        <Button type="submit" variant="primary" className="w-full" loading={reset.isPending} disabled={!token}>
          Save password
        </Button>
        {!token ? <p className="text-xs text-zinc-500">Open the link from the email to set a new password.</p> : null}
      </form>
    </AuthLayout>
  );
}

/**
 * `/confirm-email?token=…` from the confirmation email. The login's password goes with it, so the
 * address's owner can't confirm a login someone else made with their email. Signs nobody in.
 */
export function ConfirmEmailPage() {
  const [params] = useSearchParams();
  const token = params.get("token") ?? "";
  const confirm = useConfirmEmail();
  if (confirm.isSuccess) {
    return (
      <AuthLayout title="Email confirmed" subtitle="Thanks. Your DumbMail login's email address is confirmed." footer={backToLogin}>
        <ButtonLink to="/" variant="primary" className="w-full">
          Open DumbMail
        </ButtonLink>
      </AuthLayout>
    );
  }
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    confirm.mutate({ token, password: String(new FormData(e.currentTarget).get("password")) });
  };
  return (
    <AuthLayout title="Confirm your email" subtitle="Enter your DumbMail password to confirm this is your email address." footer={backToLogin}>
      <form onSubmit={onSubmit} className="space-y-3.5">
        <Field label="Password" htmlFor="confirm-password">
          <Input id="confirm-password" name="password" type="password" autoComplete="current-password" required autoFocus />
        </Field>
        <InlineError message={confirm.error ? errorMessage(confirm.error) : null} />
        <Button type="submit" variant="primary" className="w-full" loading={confirm.isPending} disabled={!token}>
          Confirm my email
        </Button>
        <p className="text-xs text-zinc-500">{token ? "Didn't make a DumbMail login? Close this page; nothing changes." : "Open the link from the email to confirm your address."}</p>
      </form>
    </AuthLayout>
  );
}

const linkExpired = "/login?error=link_expired";

/** The sign-in page's "doesn't match" answer, said for this page. */
function passwordError(error: unknown): string {
  return error instanceof ApiError && error.status === 400 ? "That password doesn't match this login." : errorMessage(error);
}

/** Whether a Google link answer says the waiting sign-in is gone (expired, used, or not this browser's). */
function linkGone(error: unknown): boolean {
  return error instanceof ApiError && error.status === 404;
}

/**
 * `/link-google`: Google sign-in found a password login for the same address whose email was never
 * confirmed. Nothing is linked until the person says whether that login is theirs.
 */
export function LinkGooglePage() {
  const link = useGoogleLink();
  const withPassword = useLinkWithPassword();
  const fresh = useStartFresh();
  const [confirmFresh, setConfirmFresh] = useState(false);

  if (link.isPending) {
    return (
      <AuthLayout title="One more step" footer={backToLogin}>
        <Skeleton className="h-40 w-full" />
      </AuthLayout>
    );
  }
  if (linkGone(link.error) || linkGone(withPassword.error) || linkGone(fresh.error)) return <Navigate to={linkExpired} replace />;
  if (link.error) {
    return (
      <AuthLayout title="Couldn't continue" footer={backToLogin}>
        <p className="text-sm text-zinc-600">{errorMessage(link.error)}</p>
      </AuthLayout>
    );
  }
  const info = link.data;
  const onPassword = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    withPassword.mutate(String(new FormData(e.currentTarget).get("password")));
  };
  return (
    <AuthLayout title="Is this login yours?" footer={backToLogin}>
      <div className="space-y-5 text-sm text-zinc-600">
        <p>
          A DumbMail login for <span className="font-medium text-zinc-900">{info.email}</span> was made {info.has_password ? "with a password " : ""}on {info.login_created_on}, and
          its email address was never confirmed. So we haven't connected your Google account to it.
        </p>
        {info.has_password ? (
          <form onSubmit={onPassword} className="space-y-3 border-t border-zinc-100 pt-4">
            <div>
              <h2 className="font-medium text-zinc-900">That was me</h2>
              <p className="mt-0.5">Enter that login's password. We'll connect Google to it and keep everything in it.</p>
            </div>
            <Field label="Password" htmlFor="link-password">
              <Input id="link-password" name="password" type="password" autoComplete="current-password" required autoFocus />
            </Field>
            <InlineError message={withPassword.error ? passwordError(withPassword.error) : null} />
            <Button type="submit" variant="primary" className="w-full" loading={withPassword.isPending}>
              Connect Google and log in
            </Button>
            <p className="text-xs text-zinc-500">
              Forgot it?{" "}
              <Link to="/forgot-password" className="font-medium text-accent-700 hover:underline">
                Set a new password
              </Link>{" "}
              from the email, then continue with Google again.
            </p>
          </form>
        ) : null}
        {info.can_start_fresh ? (
          <div className="space-y-3 border-t border-zinc-100 pt-4">
            <div>
              <h2 className="font-medium text-zinc-900">That wasn't me</h2>
              <p className="mt-0.5">Start fresh with a new, empty account of your own. The old login stops working for good.</p>
            </div>
            <InlineError message={fresh.error ? errorMessage(fresh.error) : null} />
            <Button className="w-full" onClick={() => setConfirmFresh(true)}>
              Start fresh
            </Button>
          </div>
        ) : null}
        <p className="text-xs text-zinc-500">This choice is open for {info.expires_in_minutes} minutes.</p>
      </div>
      {confirmFresh ? (
        <ConfirmDialog
          title="Start fresh?"
          danger
          confirmLabel="Start fresh"
          loading={fresh.isPending}
          body={
            <div className="space-y-2">
              <p>The old login for {info.email} is removed and can never log in again.</p>
              <p>Accounts that only it owned are closed: everyone in them loses access, and they stop sending. You get a new, empty account.</p>
            </div>
          }
          onConfirm={() => fresh.mutate(undefined, { onSettled: () => setConfirmFresh(false) })}
          onClose={() => setConfirmFresh(false)}
        />
      ) : null}
    </AuthLayout>
  );
}

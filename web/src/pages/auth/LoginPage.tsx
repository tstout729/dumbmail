import { type FormEvent } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { useLogin } from "../../api";
import { Button, Field, InlineError, Input } from "../../components/ui";
import { AuthLayout } from "./AuthLayout";
import { GoogleSignIn } from "./GoogleButton";

/** Only same-origin paths are allowed after login, so `?next=//evil.com` can't bounce users off-site. */
export function safeNextPath(next: string | null): string {
  if (!next || !next.startsWith("/")) return "/";
  try {
    const url = new URL(next, window.location.origin);
    if (url.origin !== window.location.origin) return "/";
    // Collapse leading slashes so a path like "//evil.com" can't become a protocol-relative URL.
    return url.pathname.replace(/^[/\\]+/, "/") + url.search + url.hash;
  } catch {
    return "/";
  }
}

/** What each Google sign-in error code from the server means. */
const googleErrors: Record<string, string> = {
  cancelled: "Google sign-in was cancelled.",
  expired: "Your Google sign-in expired. Try again.",
  unavailable: "Google sign-in isn't set up on this server.",
  google: "Google didn't finish the sign-in. Try again.",
  unverified: "Your Google account needs a verified email address.",
  conflict: "This email is already linked to a different Google account.",
  closed: "No account uses this Google address, and sign-ups are closed.",
  failed: "Couldn't sign you in. Try again.",
};

export function LoginPage() {
  const login = useLogin();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const next = params.get("next");
  // Set when Google sign-in bounces back here with a problem.
  const oauthError = params.get("error");

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const data = new FormData(e.currentTarget);
    login.mutate(
      { email: String(data.get("email")), password: String(data.get("password")) },
      { onSuccess: () => navigate(safeNextPath(next), { replace: true }) },
    );
  };

  return (
    <AuthLayout
      title="Log in"
      subtitle="Welcome back."
      footer={
        <>
          New to Postlark?{" "}
          <Link to="/signup" className="font-medium text-accent-700 hover:underline">
            Create an account
          </Link>
        </>
      }
    >
      <GoogleSignIn next={safeNextPath(next)} />
      <form onSubmit={onSubmit} className="mt-4 space-y-3.5">
        {oauthError && !login.error ? <InlineError message={googleErrors[oauthError] ?? googleErrors.failed} /> : null}
        <Field label="Email" htmlFor="email">
          <Input id="email" name="email" type="email" autoComplete="email" required autoFocus />
        </Field>
        <Field label="Password" htmlFor="password">
          <Input id="password" name="password" type="password" autoComplete="current-password" required />
        </Field>
        <InlineError message={login.error?.message} />
        <Button type="submit" variant="primary" className="w-full" loading={login.isPending}>
          Log in
        </Button>
      </form>
    </AuthLayout>
  );
}

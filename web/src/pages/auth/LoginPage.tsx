import { type FormEvent } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { useLogin } from "../../api";
import { Button, Field, InlineError, Input } from "../../components/ui";
import { AuthLayout } from "./AuthLayout";

/** Only same-origin paths are allowed after login, so `?next=//evil.com` can't bounce users off-site. */
export function safeNextPath(next: string | null): string {
  if (!next || !next.startsWith("/")) return "/";
  try {
    const url = new URL(next, window.location.origin);
    return url.origin === window.location.origin ? url.pathname + url.search + url.hash : "/";
  } catch {
    return "/";
  }
}

export function LoginPage() {
  const login = useLogin();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const next = params.get("next");

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
      <form onSubmit={onSubmit} className="space-y-3.5">
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

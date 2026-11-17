import { type FormEvent } from "react";
import { Link, useNavigate } from "react-router";
import { useSignup, useSiteHref } from "../../api";
import { Button, Field, InlineError, Input } from "../../components/ui";
import { AuthLayout } from "./AuthLayout";
import { GoogleSignIn } from "./GoogleButton";

export function SignupPage() {
  const signup = useSignup();
  const navigate = useNavigate();
  const terms = useSiteHref("/terms");
  const privacy = useSiteHref("/privacy");

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const data = new FormData(e.currentTarget);
    signup.mutate(
      {
        company: String(data.get("company")).trim(),
        email: String(data.get("email")).trim(),
        password: String(data.get("password")),
      },
      { onSuccess: () => navigate("/onboarding", { replace: true }) },
    );
  };

  return (
    <AuthLayout
      title="Create your account"
      subtitle="Email and SMS marketing, without the bill."
      footer={
        <>
          Already have an account?{" "}
          <Link to="/login" className="font-medium text-accent-700 hover:underline">
            Log in
          </Link>
        </>
      }
    >
      <GoogleSignIn />
      <form onSubmit={onSubmit} className="mt-4 space-y-3.5">
        <Field label="Company name" htmlFor="company">
          <Input id="company" name="company" autoComplete="organization" required autoFocus placeholder="Acme Coffee" />
        </Field>
        <Field label="Work email" htmlFor="email">
          <Input id="email" name="email" type="email" autoComplete="email" required placeholder="you@company.com" />
        </Field>
        <Field label="Password" htmlFor="password" hint="At least 8 characters.">
          <Input id="password" name="password" type="password" autoComplete="new-password" required minLength={8} />
        </Field>
        <InlineError message={signup.error?.message} />
        <Button type="submit" variant="primary" className="w-full" loading={signup.isPending}>
          Create account
        </Button>
        <p className="text-center text-xs text-zinc-500">
          By creating an account you agree to the{" "}
          <a href={terms} className="underline hover:text-zinc-700">
            Terms
          </a>{" "}
          and{" "}
          <a href={privacy} className="underline hover:text-zinc-700">
            Privacy Policy
          </a>
          .
        </p>
      </form>
    </AuthLayout>
  );
}

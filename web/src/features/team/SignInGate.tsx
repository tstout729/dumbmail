import { useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { errorMessage, keys, useLogout, useSession } from "../../api";
import { Button, buttonClass, Field, InlineError, Input } from "../../components/ui";
import { AuthLayout } from "../../pages/auth/AuthLayout";
import { useSwitchAccount, useTeamSession, useVerifyCode } from "./api";
import { TwoFactorSetupFlow } from "./TwoFactor";

/** Shown instead of the app while a session still has to finish two-factor or single sign-on. */
export function SignInGate() {
  const { account, user } = useSession();
  const { restriction, accounts } = useTeamSession();
  const logout = useLogout();
  const switcher = useSwitchAccount();
  const others = accounts.filter((a) => !a.current);
  const footer = (
    <span className="flex flex-wrap justify-center gap-x-3 gap-y-1">
      <span>Signed in as {user.email}.</span>
      <button type="button" className="font-medium text-accent-700 hover:underline" onClick={() => logout.mutate()}>
        Log out
      </button>
    </span>
  );
  const switchList =
    restriction !== "two_factor_code" && others.length > 0 ? (
      <div className="mt-5 border-t border-zinc-100 pt-4">
        <div className="mb-2 text-xs text-zinc-500">Or open another account</div>
        <div className="space-y-1">
          {others.map((a) => (
            <button
              key={a.id}
              type="button"
              disabled={switcher.isPending}
              onClick={() => switcher.mutate(a.id)}
              className="block w-full truncate rounded-md px-2 py-1.5 text-left text-sm text-zinc-700 hover:bg-zinc-100"
            >
              {a.name}
            </button>
          ))}
        </div>
      </div>
    ) : null;

  if (restriction === "two_factor_setup") {
    return (
      <AuthLayout title="Set up two-factor login" subtitle={`${account.name} requires a code from your phone when you sign in.`} footer={footer}>
        <SetupThenReload />
        {switchList}
      </AuthLayout>
    );
  }
  if (restriction === "sso_required") {
    return (
      <AuthLayout title="Sign in with single sign-on" subtitle={`${account.name} requires your company's sign-in for ${user.email}.`} footer={footer}>
        {/* A full page load: the server sends you to your identity provider. */}
        <a href={`/api/auth/sso?account=${encodeURIComponent(account.id)}`} className={buttonClass("primary", "md", "w-full")}>
          Continue with single sign-on
        </a>
        {switchList}
      </AuthLayout>
    );
  }
  return (
    <AuthLayout title="Enter your code" subtitle="Open your authenticator app and enter the 6-digit code for DumbMail." footer={footer}>
      <CodeForm />
    </AuthLayout>
  );
}

function SetupThenReload() {
  const qc = useQueryClient();
  return <TwoFactorSetupFlow onDone={() => void qc.invalidateQueries({ queryKey: keys.me })} />;
}

function CodeForm() {
  const verify = useVerifyCode();
  const [recovery, setRecovery] = useState(false);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    verify.mutate(String(new FormData(e.currentTarget).get("code")).trim());
  };
  return (
    <form onSubmit={onSubmit} className="space-y-3.5">
      <Field label={recovery ? "Recovery code" : "Code"} hint={recovery ? "One of the codes you saved when you turned on two-factor login. Each works once." : undefined}>
        {recovery ? (
          <Input key="recovery" name="code" autoComplete="off" placeholder="xxxxx-xxxxx" required autoFocus />
        ) : (
          <Input key="totp" name="code" inputMode="numeric" autoComplete="one-time-code" placeholder="123456" required autoFocus />
        )}
      </Field>
      <InlineError message={verify.error ? errorMessage(verify.error) : null} />
      <Button type="submit" variant="primary" className="w-full" loading={verify.isPending}>
        Continue
      </Button>
      <button type="button" className="text-sm font-medium text-accent-700 hover:underline" onClick={() => setRecovery((r) => !r)}>
        {recovery ? "Use the code from your app" : "Lost your phone? Use a recovery code"}
      </button>
    </form>
  );
}

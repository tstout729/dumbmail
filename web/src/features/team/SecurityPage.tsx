import { useState, type FormEvent } from "react";
import { KeyRound, LogOut, ShieldCheck, ShieldOff } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  Checkbox,
  ConfirmDialog,
  CopyField,
  Field,
  IconButton,
  InlineError,
  Input,
  Modal,
  Notice,
  Select,
  SkeletonRows,
  Table,
  TagInput,
  Td,
  Th,
  Toggle,
} from "../../components/ui";
import { formatDateTime, formatRelative } from "../../lib/format";
import {
  useAccountSecurity,
  useCan,
  useChangePassword,
  useDisableTwoFactor,
  useNewRecoveryCodes,
  useRemoveSso,
  useRevokeOtherSessions,
  useRevokeSession,
  useSaveSso,
  useSessions,
  useSso,
  useTeamSession,
  useTwoFactor,
  useUpdateSecurity,
} from "./api";
import { AuditLogCard } from "./AuditLog";
import { RecoveryCodes, TwoFactorSetupFlow } from "./TwoFactor";
import { roleLabels, type Role, type SsoSettings } from "./types";

export function SecurityPage() {
  const { sign_in_method } = useTeamSession();
  const canSecurity = useCan("security.manage");
  const isOwner = useCan("ownership.transfer");
  const canAudit = useCan("audit.read");
  const sso = sign_in_method === "sso";
  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-500">How you sign in, where you're signed in, and the account's sign-in rules and activity log.</p>
      {sso ? (
        <Notice tone="blue" icon={<ShieldCheck />} title="You signed in with single sign-on">
          Your company's identity provider handles your password and two-factor login.
        </Notice>
      ) : (
        <>
          <TwoFactorCard />
          <PasswordCard />
          <SessionsCard />
        </>
      )}
      {canSecurity ? <AccountSecurityCard /> : null}
      {isOwner ? <SsoCard /> : null}
      {canAudit ? <AuditLogCard /> : null}
    </div>
  );
}

function TwoFactorCard() {
  const status = useTwoFactor();
  const [setting, setSetting] = useState(false);
  const [codeFor, setCodeFor] = useState<"disable" | "codes" | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const s = status.data;
  return (
    <Card title="Your two-factor login">
      {status.isPending ? (
        <div className="h-10" />
      ) : s?.enabled ? (
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="text-sm text-zinc-600">
            <Badge tone="green">On</Badge> <span className="ml-1">Signing in with your password also asks for a code from your app.</span>
            <div className="mt-1 text-xs text-zinc-500">{s.recovery_codes_left} recovery codes left.</div>
            {s.required_by.length > 0 ? <div className="mt-1 text-xs text-zinc-500">Required by {s.required_by.join(", ")}.</div> : null}
          </div>
          <div className="flex flex-wrap gap-2">
            <Button size="sm" onClick={() => setCodeFor("codes")}>
              New recovery codes
            </Button>
            {s.required_by.length === 0 ? (
              <Button size="sm" variant="danger" icon={<ShieldOff className="size-3.5" />} onClick={() => setCodeFor("disable")}>
                Turn off
              </Button>
            ) : null}
          </div>
        </div>
      ) : (
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-sm text-zinc-600">Add a code from your phone to every password sign-in, so a stolen password isn't enough.</p>
          <Button variant="primary" size="sm" icon={<ShieldCheck className="size-3.5" />} onClick={() => setSetting(true)}>
            Set up
          </Button>
        </div>
      )}
      {setting ? (
        <Modal title="Set up two-factor login" onClose={() => setSetting(false)}>
          <TwoFactorSetupFlow onDone={() => setSetting(false)} />
        </Modal>
      ) : null}
      {codeFor ? <CodeModal mode={codeFor} onClose={() => setCodeFor(null)} onCodes={(c) => { setCodeFor(null); setCodes(c); }} /> : null}
      {codes ? (
        <Modal title="Your new recovery codes" onClose={() => setCodes(null)}>
          <RecoveryCodes codes={codes} onDone={() => setCodes(null)} />
        </Modal>
      ) : null}
    </Card>
  );
}

function CodeModal({ mode, onClose, onCodes }: { mode: "disable" | "codes"; onClose: () => void; onCodes: (codes: string[]) => void }) {
  const disable = useDisableTwoFactor();
  const regen = useNewRecoveryCodes();
  const pending = disable.isPending || regen.isPending;
  const error = disable.error ?? regen.error;
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const code = String(new FormData(e.currentTarget).get("code"));
    if (mode === "disable") disable.mutate(code, { onSuccess: onClose });
    else regen.mutate(code, { onSuccess: (r) => onCodes(r.recovery_codes) });
  };
  return (
    <Modal title={mode === "disable" ? "Turn off two-factor login" : "Make new recovery codes"} onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <p className="text-sm text-zinc-600">{mode === "disable" ? "Enter a code from your app to confirm." : "Your old recovery codes stop working. Enter a code from your app to confirm."}</p>
        <Field label="Code">
          <Input name="code" autoComplete="one-time-code" required autoFocus />
        </Field>
        <InlineError message={error ? errorMessage(error) : null} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant={mode === "disable" ? "danger" : "primary"} loading={pending}>
            {mode === "disable" ? "Turn off" : "Make new codes"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function PasswordCard() {
  const change = useChangePassword();
  const [done, setDone] = useState<number | null>(null);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const form = e.currentTarget;
    const data = new FormData(form);
    change.mutate(
      { current_password: String(data.get("current")), new_password: String(data.get("new")) },
      {
        onSuccess: (r) => {
          form.reset();
          setDone(r.other_sessions_revoked);
        },
      },
    );
  };
  return (
    <Card title="Password">
      <form onSubmit={onSubmit} className="grid gap-3 sm:grid-cols-[1fr_1fr_auto] sm:items-start">
        <Field label="Current password" hint=" ">
          <Input name="current" type="password" autoComplete="current-password" required />
        </Field>
        <Field label="New password" hint="At least 8 characters.">
          <Input name="new" type="password" autoComplete="new-password" minLength={8} required />
        </Field>
        <Button type="submit" variant="primary" loading={change.isPending} className="sm:mt-5">
          Change password
        </Button>
      </form>
      <div className="mt-2">
        <InlineError message={change.error ? errorMessage(change.error) : null} />
        {done !== null ? <p className="text-sm text-accent-800">Password changed. Signed out {done === 1 ? "1 other session" : `${done} other sessions`}.</p> : null}
      </div>
    </Card>
  );
}

function SessionsCard() {
  const { account } = useSession();
  const sessions = useSessions();
  const revoke = useRevokeSession();
  const others = useRevokeOtherSessions();
  const rows = sessions.data ?? [];
  return (
    <Card
      title="Where you're signed in"
      padded={false}
      actions={
        rows.length > 1 ? (
          <Button size="sm" icon={<LogOut className="size-3.5" />} loading={others.isPending} onClick={() => others.mutate()}>
            Sign out other sessions
          </Button>
        ) : null
      }
    >
      <div className="overflow-x-auto">
        <Table className="rounded-none border-0 shadow-none">
          <thead>
            <tr>
              <Th>Device</Th>
              <Th className="hidden sm:table-cell">Address</Th>
              <Th className="hidden sm:table-cell">Last active</Th>
              <Th className="hidden md:table-cell">Signed in</Th>
              <Th className="w-10" />
            </tr>
          </thead>
          <tbody>
            {sessions.isPending ? (
              <SkeletonRows cols={5} rows={2} />
            ) : (
              rows.map((s) => (
                <tr key={s.id}>
                  <Td>
                    <div className="font-medium text-zinc-900">{s.device}</div>
                    <div className="text-xs text-zinc-500">
                      {s.account}
                      {s.current ? " · this browser" : ""}
                    </div>
                    {s.current ? null : <div className="text-xs text-zinc-500 sm:hidden">Active {formatRelative(s.last_seen_at)}</div>}
                  </Td>
                  <Td className="hidden font-mono text-xs text-zinc-600 sm:table-cell">{s.ip ?? "—"}</Td>
                  <Td className="hidden text-zinc-600 sm:table-cell">{s.current ? <Badge tone="green">Now</Badge> : formatRelative(s.last_seen_at)}</Td>
                  <Td className="hidden text-zinc-600 md:table-cell">{formatDateTime(s.created_at, account.timezone)}</Td>
                  <Td>
                    <IconButton label={s.current ? "Sign out here" : `Sign out ${s.device}`} className="hover:text-red-600" onClick={() => revoke.mutate(s.id)}>
                      <LogOut className="size-3.5" />
                    </IconButton>
                  </Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      </div>
    </Card>
  );
}

function AccountSecurityCard() {
  const { account } = useSession();
  const { two_factor_enabled } = useTeamSession();
  const security = useAccountSecurity();
  const update = useUpdateSecurity();
  const s = security.data;
  return (
    <Card title={`Sign-in rules for ${account.name}`}>
      <div className="flex items-start justify-between gap-4">
        <div className="text-sm">
          <div className="font-medium text-zinc-900">Require two-factor login for everyone</div>
          <p className="text-zinc-600">
            Members without it set it up the next time they use {account.name}. This includes people who sign in with Google. Single sign-on counts on its own.
          </p>
          {s && s.members_without_two_factor > 0 ? (
            <p className="mt-1 text-xs text-zinc-500">{s.members_without_two_factor === 1 ? "1 member doesn't" : `${s.members_without_two_factor} members don't`} have it yet.</p>
          ) : null}
          {!two_factor_enabled && !s?.require_two_factor ? <p className="mt-1 text-xs text-amber-700">Turn it on for yourself first.</p> : null}
        </div>
        <Toggle
          label="Require two-factor login for everyone"
          checked={s?.require_two_factor ?? false}
          disabled={!s || update.isPending}
          onChange={(v) => update.mutate(v)}
        />
      </div>
    </Card>
  );
}

const ssoRoles: Role[] = ["manager", "analyst", "content"];

function SsoCard() {
  const sso = useSso();
  const [editing, setEditing] = useState(false);
  const [removing, setRemoving] = useState(false);
  const remove = useRemoveSso();
  const current = sso.data?.sso ?? null;
  return (
    <Card
      title="Single sign-on"
      actions={
        current && !editing ? (
          <div className="flex gap-2">
            <Button size="sm" onClick={() => setEditing(true)}>
              Edit
            </Button>
            <Button size="sm" variant="danger" onClick={() => setRemoving(true)}>
              Remove
            </Button>
          </div>
        ) : null
      }
    >
      {sso.isPending ? (
        <div className="h-10" />
      ) : editing ? (
        <SsoForm current={current} redirectUri={sso.data?.redirect_uri ?? ""} onDone={() => setEditing(false)} />
      ) : current ? (
        <SsoSummary sso={current} />
      ) : (
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-sm text-zinc-600">Let people at your company sign in with Okta, Microsoft Entra ID, Google Workspace or any OpenID Connect provider.</p>
          <Button size="sm" variant="primary" icon={<KeyRound className="size-3.5" />} onClick={() => setEditing(true)}>
            Set up
          </Button>
        </div>
      )}
      {removing ? (
        <ConfirmDialog
          title="Remove single sign-on?"
          body="People who only sign in this way can't get in until you set it up again. Their sessions stay signed in until they log out."
          confirmLabel="Remove"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(undefined, { onSettled: () => setRemoving(false) })}
          onClose={() => setRemoving(false)}
        />
      ) : null}
    </Card>
  );
}

function SsoSummary({ sso }: { sso: SsoSettings }) {
  return (
    <dl className="grid gap-3 text-sm sm:grid-cols-2">
      <div>
        <dt className="text-xs text-zinc-500">Identity provider</dt>
        <dd className="break-all text-zinc-900">{sso.issuer}</dd>
      </div>
      <div>
        <dt className="text-xs text-zinc-500">Email domains</dt>
        <dd className="text-zinc-900">{sso.domains.join(", ")}</dd>
      </div>
      <div>
        <dt className="text-xs text-zinc-500">New people</dt>
        <dd className="text-zinc-900">{sso.auto_join ? `Join as ${roleLabels[sso.default_role]}` : "Must be invited first"}</dd>
      </div>
      <div>
        <dt className="text-xs text-zinc-500">Status</dt>
        <dd className="flex flex-wrap gap-1.5">
          {sso.enabled ? <Badge tone="green">On</Badge> : <Badge>Off</Badge>}
          {sso.enforce ? <Badge tone="blue">Required for these domains</Badge> : null}
          {sso.verified_emails_only ? <Badge>Trusts its emails as verified</Badge> : null}
          {sso.idp_enforces_mfa ? <Badge>Counts as two-factor</Badge> : null}
        </dd>
      </div>
      <div className="sm:col-span-2">
        <dt className="mb-1 text-xs text-zinc-500">Sign-in link</dt>
        <CopyField value={sso.login_url} />
      </div>
    </dl>
  );
}

function SsoForm({ current, redirectUri, onDone }: { current: SsoSettings | null; redirectUri: string; onDone: () => void }) {
  const save = useSaveSso();
  const [domains, setDomains] = useState<string[]>(current?.domains ?? []);
  const [role, setRole] = useState<Role>(current?.default_role ?? "analyst");
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const secret = String(d.get("client_secret")).trim();
    save.mutate(
      {
        issuer: String(d.get("issuer")).trim(),
        client_id: String(d.get("client_id")).trim(),
        client_secret: secret || undefined,
        domains,
        default_role: role,
        auto_join: d.get("auto_join") === "on",
        enforce: d.get("enforce") === "on",
        enabled: d.get("enabled") === "on",
        verified_emails_only: d.get("verified_emails_only") === "on",
        idp_enforces_mfa: d.get("idp_enforces_mfa") === "on",
      },
      { onSuccess: onDone },
    );
  };
  return (
    <form onSubmit={onSubmit} className="space-y-4">
      <div>
        <div className="mb-1 text-xs font-medium text-zinc-700">Redirect URI to add in your identity provider</div>
        <CopyField value={redirectUri} />
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Issuer URL" hint="From your provider, like https://acme.okta.com">
          <Input name="issuer" type="url" required defaultValue={current?.issuer} placeholder="https://" />
        </Field>
        <Field label="Client ID">
          <Input name="client_id" required defaultValue={current?.client_id} />
        </Field>
        <Field label="Client secret" hint={current ? `Saved (ends ${current.client_secret_hint.replace("…", "")}). Leave blank to keep it.` : "Stored encrypted. You won't see it again."}>
          <Input name="client_secret" type="password" autoComplete="off" required={!current} />
        </Field>
        <Field label="Email domains" hint="Each must be a verified sending domain. Press Enter after each.">
          <TagInput value={domains} onChange={setDomains} placeholder="acme.com" aria-label="Email domains" />
        </Field>
        <Field label="Role for people who join this way">
          <Select value={role} onChange={(e) => setRole(e.target.value as Role)}>
            {ssoRoles.map((r) => (
              <option key={r} value={r}>
                {roleLabels[r]}
              </option>
            ))}
          </Select>
        </Field>
      </div>
      <div className="space-y-2">
        <Checkbox name="auto_join" defaultChecked={current?.auto_join ?? true} label="Add new people from these domains when they first sign in" />
        <Checkbox name="enforce" defaultChecked={current?.enforce ?? false} label="Require single sign-on for these domains" description="Owners can still use their password, so a broken provider can't lock everyone out." />
        <Checkbox
          name="verified_emails_only"
          defaultChecked={current?.verified_emails_only ?? false}
          label="This provider only issues verified emails"
          description="Tick only if your provider never sends email_verified. Otherwise people are linked to an existing login only when the provider says their email is verified."
        />
        <Checkbox
          name="idp_enforces_mfa"
          defaultChecked={current?.idp_enforces_mfa ?? false}
          label="Our identity provider enforces two-step verification"
          description="When this account requires two-factor login, sign-ins through the provider count as two-factor. Otherwise they count only when the provider says a second factor was used, and people also enter their DumbMail code."
        />
        <Checkbox name="enabled" defaultChecked={current?.enabled ?? true} label="Turned on" description="Turning it off, or changing the provider, signs out everyone who signed in through it." />
      </div>
      <InlineError message={save.error ? errorMessage(save.error) : null} />
      <div className="flex justify-end gap-2">
        <Button variant="ghost" onClick={onDone}>
          Cancel
        </Button>
        <Button type="submit" variant="primary" loading={save.isPending}>
          Save
        </Button>
      </div>
    </form>
  );
}

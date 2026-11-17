import { useState, type FormEvent } from "react";
import { KeyRound, MailPlus, MoreHorizontal, RefreshCw, ShieldCheck, Trash2, UserMinus, Users } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  CopyField,
  Field,
  IconButton,
  InlineError,
  Input,
  Menu,
  Modal,
  Notice,
  Select,
  SkeletonRows,
  Table,
  Td,
  Th,
} from "../../components/ui";
import { formatDate, formatRelative } from "../../lib/format";
import {
  useCan,
  useChangeRole,
  useInvite,
  useLeaveAccount,
  useRemoveMember,
  useResendInvite,
  useResetSsoLink,
  useRevokeInvite,
  useTeam,
  useTeamSession,
  useTransferOwnership,
} from "./api";
import { ConfirmIdentityModal } from "./ConfirmIdentity";
import { roleDescriptions, roleLabels, roles, type Invite, type InviteResult, type Member, type Role } from "./types";

/** A role change that makes or demotes an owner, waiting for the password or code. */
type OwnerChange = { member: Member; role: Role };

export function TeamPage() {
  const { user, account } = useSession();
  const { role: myRole } = useTeamSession();
  const canManage = useCan("team.manage");
  const team = useTeam();
  const changeRole = useChangeRole();
  const resend = useResendInvite();
  const revoke = useRevokeInvite();
  const [inviting, setInviting] = useState(false);
  const [removing, setRemoving] = useState<Member | null>(null);
  const [transferring, setTransferring] = useState<Member | null>(null);
  const [leaving, setLeaving] = useState(false);
  const [resent, setResent] = useState<InviteResult | null>(null);
  const [ownerChange, setOwnerChange] = useState<OwnerChange | null>(null);
  const [resendingOwner, setResendingOwner] = useState<Invite | null>(null);
  const [resettingSso, setResettingSso] = useState<Member | null>(null);
  const remove = useRemoveMember();
  const transfer = useTransferOwnership();
  const leave = useLeaveAccount();
  const resetSso = useResetSsoLink();
  const tz = account.timezone;
  const members = team.data?.members ?? [];
  const invites = team.data?.invites ?? [];
  const assignable = roles.filter((r) => r !== "owner" || myRole === "owner");
  // Only owners change owners.
  const canEdit = (m: Member) => canManage && m.user_id !== user.id && (m.role !== "owner" || myRole === "owner");

  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-500">Who can use {account.name}, and what each person can do. Every role is checked on the server, not just hidden here.</p>
      {team.error ? <InlineError message={errorMessage(team.error)} /> : null}
      <Card
        title="Members"
        padded={false}
        actions={
          canManage ? (
            <Button size="sm" variant="primary" icon={<MailPlus className="size-3.5" />} onClick={() => setInviting(true)}>
              Invite
            </Button>
          ) : null
        }
      >
        <div className="overflow-x-auto">
          <Table className="rounded-none border-0 shadow-none">
            <thead>
              <tr>
                <Th>Person</Th>
                <Th>Role</Th>
                <Th className="hidden sm:table-cell">Two-factor</Th>
                <Th className="hidden md:table-cell">Last active</Th>
                <Th className="w-10" />
              </tr>
            </thead>
            <tbody>
              {team.isPending ? (
                <SkeletonRows cols={5} rows={2} />
              ) : (
                members.map((m) => (
                  <tr key={m.user_id}>
                    <Td>
                      <div className="font-medium text-zinc-900">{m.name || m.email}</div>
                      {m.name ? <div className="text-xs text-zinc-500">{m.email}</div> : null}
                      {m.user_id === user.id ? <div className="text-xs text-zinc-400">You</div> : null}
                    </Td>
                    <Td>
                      {canEdit(m) ? (
                        <Select
                          aria-label={`Role for ${m.email}`}
                          className="w-28"
                          value={m.role}
                          disabled={changeRole.isPending}
                          onChange={(e) => {
                            const role = e.target.value as Role;
                            // Making or demoting an owner asks for the password or a code first.
                            if (role === "owner" || m.role === "owner") setOwnerChange({ member: m, role });
                            else changeRole.mutate({ userId: m.user_id, role });
                          }}
                        >
                          {assignable.map((r) => (
                            <option key={r} value={r}>
                              {roleLabels[r]}
                            </option>
                          ))}
                        </Select>
                      ) : (
                        <Badge tone={m.role === "owner" ? "green" : "gray"}>{roleLabels[m.role]}</Badge>
                      )}
                    </Td>
                    <Td className="hidden sm:table-cell">{m.two_factor ? <Badge tone="green">On</Badge> : <span className="text-sm text-zinc-500">Off</span>}</Td>
                    <Td className="hidden text-zinc-600 md:table-cell">{formatRelative(m.last_active_at)}</Td>
                    <Td>
                      <MemberMenu
                        member={m}
                        canRemove={canEdit(m)}
                        canTransfer={myRole === "owner" && m.user_id !== user.id}
                        canResetSso={myRole === "owner" && m.sso_linked}
                        onRemove={() => setRemoving(m)}
                        onTransfer={() => setTransferring(m)}
                        onResetSso={() => setResettingSso(m)}
                      />
                    </Td>
                  </tr>
                ))
              )}
            </tbody>
          </Table>
        </div>
      </Card>

      {invites.length > 0 ? (
        <Card title="Open invites" padded={false}>
          <div className="overflow-x-auto">
            <Table className="rounded-none border-0 shadow-none">
              <thead>
                <tr>
                  <Th>Email</Th>
                  <Th className="hidden sm:table-cell">Role</Th>
                  <Th className="hidden sm:table-cell">Sent</Th>
                  <Th>Expires</Th>
                  {canManage ? <Th className="w-20" /> : null}
                </tr>
              </thead>
              <tbody>
                {invites.map((i) => (
                  <tr key={i.id}>
                    <Td>
                      <div className="font-medium text-zinc-900">{i.email}</div>
                      <div className="text-xs text-zinc-500 sm:hidden">{roleLabels[i.role]}</div>
                    </Td>
                    <Td className="hidden sm:table-cell">{roleLabels[i.role]}</Td>
                    <Td className="hidden text-zinc-600 sm:table-cell">{formatDate(i.sent_at, tz)}</Td>
                    <Td>{i.expired ? <Badge tone="amber">Expired</Badge> : <span className="text-zinc-600">{formatDate(i.expires_at, tz)}</span>}</Td>
                    {canManage ? (
                      <Td>
                        <div className="flex justify-end gap-1">
                          <IconButton
                            label={`Resend invite to ${i.email}`}
                            onClick={() => (i.role === "owner" ? setResendingOwner(i) : resend.mutate(i.id, { onSuccess: setResent }))}
                          >
                            <RefreshCw className="size-3.5" />
                          </IconButton>
                          <IconButton label={`Revoke invite to ${i.email}`} className="hover:text-red-600" onClick={() => revoke.mutate(i.id)}>
                            <Trash2 className="size-3.5" />
                          </IconButton>
                        </div>
                      </Td>
                    ) : null}
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
        </Card>
      ) : null}

      <Card title="Roles">
        <dl className="grid gap-3 text-sm sm:grid-cols-2">
          {roles.map((r) => (
            <div key={r}>
              <dt className="font-medium text-zinc-900">{roleLabels[r]}</dt>
              <dd className="text-zinc-600">{roleDescriptions[r]}</dd>
            </div>
          ))}
        </dl>
      </Card>

      <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-zinc-200 bg-white px-4 py-3 shadow-xs">
        <div className="text-sm">
          <div className="font-medium text-zinc-900">Leave {account.name}</div>
          <div className="text-zinc-500">You lose access to this account. Your other accounts stay as they are.</div>
        </div>
        <Button variant="danger" size="sm" icon={<UserMinus className="size-3.5" />} onClick={() => setLeaving(true)}>
          Leave account
        </Button>
      </div>

      {inviting ? <InviteModal assignable={assignable} onClose={() => setInviting(false)} /> : null}
      {resent ? <InviteSentModal result={resent} onClose={() => setResent(null)} /> : null}
      {removing?.role === "owner" ? (
        <ConfirmIdentityModal
          title={`Remove ${removing.name || removing.email}?`}
          body="They lose access to this account right away, and the API keys they made here stop working. Removing an owner needs your password or a code."
          confirmLabel="Remove"
          danger
          onConfirmed={() => remove.mutateAsync(removing.user_id)}
          onClose={() => setRemoving(null)}
        />
      ) : removing ? (
        <ConfirmDialog
          title={`Remove ${removing.name || removing.email}?`}
          body="They lose access to this account right away, and the API keys they made here stop working."
          confirmLabel="Remove"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(removing.user_id, { onSettled: () => setRemoving(null) })}
          onClose={() => setRemoving(null)}
        />
      ) : null}
      {transferring ? (
        <ConfirmIdentityModal
          title={`Make ${transferring.name || transferring.email} the owner?`}
          body="They become the owner, and you become an admin. Only an owner can undo this."
          confirmLabel="Transfer ownership"
          danger
          onConfirmed={() => transfer.mutateAsync(transferring.user_id)}
          onClose={() => setTransferring(null)}
        />
      ) : null}
      {ownerChange ? (
        <ConfirmIdentityModal
          title={ownerChange.role === "owner" ? `Make ${ownerChange.member.name || ownerChange.member.email} an owner?` : `Change ${ownerChange.member.name || ownerChange.member.email} to ${roleLabels[ownerChange.role]}?`}
          body={ownerChange.role === "owner" ? "Owners can do everything, including billing, ownership and deleting the account." : "They stop being an owner."}
          confirmLabel="Change role"
          onConfirmed={() => changeRole.mutateAsync({ userId: ownerChange.member.user_id, role: ownerChange.role })}
          onClose={() => setOwnerChange(null)}
        />
      ) : null}
      {resendingOwner ? (
        <ConfirmIdentityModal
          title={`Resend the owner invite to ${resendingOwner.email}?`}
          body="They get a new link; the old one stops working."
          confirmLabel="Resend invite"
          onConfirmed={() => resend.mutateAsync(resendingOwner.id).then(setResent)}
          onClose={() => setResendingOwner(null)}
        />
      ) : null}
      {resettingSso ? (
        <ConfirmDialog
          title={`Reset ${resettingSso.name || resettingSso.email}'s single sign-on link?`}
          body="Their single sign-on sessions here end. The next time they sign in with SSO, their login is linked again by their verified email. Use this when your identity provider gave them a new account."
          confirmLabel="Reset link"
          loading={resetSso.isPending}
          onConfirm={() => resetSso.mutate(resettingSso.user_id, { onSettled: () => setResettingSso(null) })}
          onClose={() => setResettingSso(null)}
        />
      ) : null}
      {leaving && myRole === "owner" ? (
        <ConfirmIdentityModal
          title={`Leave ${account.name}?`}
          body="Someone will have to invite you again to get back in. Leaving as an owner needs your password or a code."
          confirmLabel="Leave account"
          danger
          onConfirmed={() => leave.mutateAsync(undefined)}
          onClose={() => setLeaving(false)}
        />
      ) : leaving ? (
        <ConfirmDialog
          title={`Leave ${account.name}?`}
          body="Someone will have to invite you again to get back in."
          confirmLabel="Leave account"
          danger
          loading={leave.isPending}
          onConfirm={() => leave.mutate(undefined, { onSettled: () => setLeaving(false) })}
          onClose={() => setLeaving(false)}
        />
      ) : null}
    </div>
  );
}

type MemberMenuProps = {
  member: Member;
  canRemove: boolean;
  canTransfer: boolean;
  canResetSso: boolean;
  onRemove: () => void;
  onTransfer: () => void;
  onResetSso: () => void;
};

function MemberMenu({ member, canRemove, canTransfer, canResetSso, onRemove, onTransfer, onResetSso }: MemberMenuProps) {
  const items = [
    ...(canTransfer ? [{ label: "Make owner (transfer)", icon: <ShieldCheck className="size-3.5" />, onSelect: onTransfer }] : []),
    ...(canResetSso ? [{ label: "Reset single sign-on link", icon: <KeyRound className="size-3.5" />, onSelect: onResetSso }] : []),
    ...(canRemove ? [{ label: "Remove from account", icon: <Trash2 className="size-3.5" />, onSelect: onRemove, danger: true }] : []),
  ];
  if (items.length === 0) return null;
  return (
    <Menu
      trigger={(props) => (
        <IconButton label={`Actions for ${member.email}`} {...props}>
          <MoreHorizontal className="size-4" />
        </IconButton>
      )}
      items={items}
    />
  );
}

function InviteModal({ assignable, onClose }: { assignable: Role[]; onClose: () => void }) {
  const invite = useInvite();
  const [role, setRole] = useState<Role>("manager");
  const [result, setResult] = useState<InviteResult | null>(null);
  const [ownerEmail, setOwnerEmail] = useState<string | null>(null);

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const email = String(new FormData(e.currentTarget).get("email")).trim();
    // Inviting an owner asks for the password or a code first.
    if (role === "owner") setOwnerEmail(email);
    else invite.mutate({ email, role }, { onSuccess: setResult });
  };

  if (result) return <InviteSentModal result={result} onClose={onClose} />;
  if (ownerEmail)
    return (
      <ConfirmIdentityModal
        title={`Invite ${ownerEmail} as an owner?`}
        body="Owners can do everything, including billing, ownership and deleting the account."
        confirmLabel="Send invite"
        onConfirmed={() => invite.mutateAsync({ email: ownerEmail, role: "owner" }).then(setResult)}
        onClose={() => setOwnerEmail(null)}
      />
    );
  return (
    <Modal title="Invite a teammate" onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Email">
          <Input name="email" type="email" required autoFocus placeholder="name@company.com" />
        </Field>
        <Field label="Role" hint={roleDescriptions[role]}>
          <Select value={role} onChange={(e) => setRole(e.target.value as Role)}>
            {assignable.map((r) => (
              <option key={r} value={r}>
                {roleLabels[r]}
              </option>
            ))}
          </Select>
        </Field>
        <InlineError message={invite.error ? errorMessage(invite.error) : null} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={invite.isPending}>
            Send invite
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function InviteSentModal({ result, onClose }: { result: InviteResult; onClose: () => void }) {
  return (
    <Modal
      title={result.email_sent ? "Invite sent" : "Invite created"}
      onClose={onClose}
      size="sm"
      footer={
        <Button variant="primary" onClick={onClose}>
          Done
        </Button>
      }
    >
      <div className="space-y-3 text-sm text-zinc-600">
        {result.email_sent ? (
          <p>
            We emailed {result.invite.email} a link to join as {roleLabels[result.invite.role]}. It works for 7 days.
          </p>
        ) : (
          <>
            <Notice tone="amber" icon={<Users />} title="This server can't send email">
              Send this link to {result.invite.email} yourself. It works for 7 days.
            </Notice>
            {result.invite_url ? <CopyField value={result.invite_url} /> : null}
          </>
        )}
      </div>
    </Modal>
  );
}

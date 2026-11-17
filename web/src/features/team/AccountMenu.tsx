import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router";
import { Check, ChevronsUpDown, LogOut, Plus, Settings } from "lucide-react";
import { errorMessage, useLogout, useSession } from "../../api";
import { Button, Field, InlineError, Input, Menu, Modal } from "../../components/ui";
import { useCreateAccount, useSwitchAccount, useTeamSession } from "./api";

/** The sidebar's account button: switch between this login's accounts, start a new one, log out. */
export function AccountMenu() {
  const { user, account } = useSession();
  const { accounts, sign_in_method } = useTeamSession();
  const logout = useLogout();
  const navigate = useNavigate();
  const switcher = useSwitchAccount();
  const [creating, setCreating] = useState(false);
  // A single sign-on session opens only its own account.
  const sso = sign_in_method === "sso";

  // Menu items are keyed by label, so same-named accounts get a number.
  const seen = new Map<string, number>();
  const items = [
    ...accounts.map((a) => {
      const n = (seen.get(a.name) ?? 0) + 1;
      seen.set(a.name, n);
      return { account: a, label: n > 1 ? `${a.name} (${n})` : a.name };
    }).map(({ account: a, label }) => ({
      label,
      icon: a.current ? <Check className="size-3.5 text-accent-700" /> : <span className="size-3.5" />,
      onSelect: () => {
        if (!a.current) switcher.mutate(a.id);
      },
    })),
    ...(sso ? [] : [{ label: "New account", icon: <Plus className="size-3.5" />, onSelect: () => setCreating(true) }]),
    { label: "Account settings", icon: <Settings className="size-3.5" />, onSelect: () => navigate("/settings") },
    { label: "Log out", icon: <LogOut className="size-3.5" />, onSelect: () => logout.mutate() },
  ];

  return (
    <>
      <Menu
        align="left"
        up
        className="relative block w-full"
        trigger={(props) => (
          <button type="button" {...props} className="flex w-full items-center gap-2.5 rounded-md px-2 py-1.5 text-left hover:bg-zinc-200/60">
            <span className="flex size-7 shrink-0 items-center justify-center rounded-md text-xs font-semibold text-white" style={{ background: account.brand_color || "#115f42" }}>
              {account.name.slice(0, 1).toUpperCase()}
            </span>
            <span className="min-w-0 flex-1">
              <span className="block truncate text-[13px] font-medium text-zinc-900">{account.name}</span>
              <span className="block truncate text-[11px] text-zinc-500">{user.email}</span>
            </span>
            <ChevronsUpDown className="size-3.5 text-zinc-400" />
          </button>
        )}
        items={items}
      />
      {creating ? <NewAccountModal onClose={() => setCreating(false)} /> : null}
    </>
  );
}

function NewAccountModal({ onClose }: { onClose: () => void }) {
  const create = useCreateAccount();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    create.mutate(String(new FormData(e.currentTarget).get("name")).trim());
  };
  return (
    <Modal title="New account" description="A separate brand or store with its own audience, campaigns and settings. You'll be its owner." onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Brand or company name">
          <Input name="name" required autoFocus maxLength={100} />
        </Field>
        <InlineError message={create.error ? errorMessage(create.error) : null} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Create account
          </Button>
        </div>
      </form>
    </Modal>
  );
}

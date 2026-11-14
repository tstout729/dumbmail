import type { AccountPatch } from "../../api";
import { useUpdateAccount } from "../../api";
import { toast } from "../../lib/toast";

export function useSaveAccount() {
  const update = useUpdateAccount();
  const save = (patch: AccountPatch) => update.mutate(patch, { onSuccess: () => toast.success("Settings saved") });
  return { save, saving: update.isPending };
}

/** Trimmed form value, or null when empty. */
export const formString = (d: FormData, k: string) => String(d.get(k) ?? "").trim() || null;

export function hostnameOf(url: string | null): string {
  if (!url) return "";
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return "";
  }
}

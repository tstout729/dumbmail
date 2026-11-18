import { useCan } from "../team/api";
import { useUnread } from "./api";

/** The sidebar's unread count: conversations with texts nobody has read yet. */
export function UnreadBadge() {
  const unread = useUnread(useCan("profiles.read"));
  const n = unread.data?.conversations ?? 0;
  if (n === 0) return null;
  return (
    <span className="ml-auto rounded-full bg-accent-700 px-1.5 text-[11px] font-semibold text-white tabular-nums" aria-label={`${n} unread conversations`}>
      {n > 99 ? "99+" : n}
    </span>
  );
}

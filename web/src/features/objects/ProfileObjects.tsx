import { Link } from "react-router";
import { useSession } from "../../api";
import { Badge, Card } from "../../components/ui";
import { useCan } from "../team/api";
import { useProfileObjects } from "./api";
import { fieldLabel, formatValue } from "./parts";

/** A profile's objects (reservations, subscriptions ...), by type, with their key fields. */
export function ProfileObjects({ profileId }: { profileId: string }) {
  const { account } = useSession();
  const can = useCan("profiles.read");
  const q = useProfileObjects(profileId, can);
  const groups = q.data?.types ?? [];
  if (!can || q.isPending || q.error || groups.length === 0) return null;
  return (
    <>
      {groups.map((g) => (
        <Card key={g.key} title={g.plural} padded={false}>
          <ul className="divide-y divide-zinc-100">
            {g.objects.map((o) => (
              <li key={`${o.id}-${o.relationship}`}>
                <Link to={`/audience/objects/${g.key}/${o.id}`} className="block px-4 py-3 hover:bg-zinc-50">
                  <div className="flex items-center justify-between gap-2">
                    <span className="truncate text-sm font-medium text-zinc-900">{o.title || o.external_id}</span>
                    {o.relationship ? <Badge>{o.relationship}</Badge> : null}
                  </div>
                  <div className="mt-0.5 flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-zinc-500">
                    {g.fields
                      .filter((f) => f.key in o.fields)
                      .map((f) => (
                        <span key={f.key}>
                          {fieldLabel(f)}: <span className="text-zinc-700">{formatValue(f, o.fields[f.key], account.timezone)}</span>
                        </span>
                      ))}
                  </div>
                </Link>
              </li>
            ))}
          </ul>
        </Card>
      ))}
    </>
  );
}

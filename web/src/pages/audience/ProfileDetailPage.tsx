import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { Activity, ListChecks, Mail, MessageSquare, MousePointerClick, MailOpen, Pencil, PieChart, Plus, Trash2, X } from "lucide-react";
import type { Consent, Profile, ProfileDetail } from "../../api";
import { useDeleteProfile, useProfile, useSession, useUpdateProfile } from "../../api";
import { Badge, Button, Card, ConfirmDialog, EmptyState, Field, IconButton, Input, Page, PageHeader, Select, Skeleton, StatusBadge } from "../../components/ui";
import { formatDateTime, formatPhone, formatRelative, fullName } from "../../lib/format";
import { toast } from "../../lib/toast";

export function ProfileDetailPage() {
  const { id = "" } = useParams();
  const detail = useProfile(id);
  if (detail.isPending) {
    return (
      <Page>
        <Skeleton className="mb-2 h-7 w-64" />
        <Skeleton className="mb-6 h-4 w-48" />
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-[340px_minmax(0,1fr)]">
          <Skeleton className="h-80" />
          <Skeleton className="h-80" />
        </div>
      </Page>
    );
  }
  if (detail.error) {
    return (
      <Page>
        <PageHeader title="Profile" breadcrumbs={[{ label: "Profiles", to: "/audience/profiles" }]} />
        <p className="text-sm text-red-600">{detail.error.message}</p>
      </Page>
    );
  }
  return <ProfileView key={detail.data.profile.id} detail={detail.data} />;
}

function ProfileView({ detail }: { detail: ProfileDetail }) {
  const { account } = useSession();
  const { profile, lists, segments, events, messages } = detail;
  const [editing, setEditing] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const remove = useDeleteProfile();
  const navigate = useNavigate();
  const tz = account.timezone;
  const title = fullName(profile) || profile.email || profile.phone_number || "Anonymous profile";

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Profiles", to: "/audience/profiles" }]}
        title={
          <span className="flex items-center gap-3">
            <span className="flex size-9 items-center justify-center rounded-full bg-accent-100 text-sm font-semibold text-accent-800">
              {(profile.first_name ?? profile.email ?? "?").slice(0, 1).toUpperCase()}
            </span>
            {title}
          </span>
        }
        description={
          <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
            {profile.email ? <span>{profile.email}</span> : null}
            {profile.phone_number ? <span className="tabular-nums">{formatPhone(profile.phone_number)}</span> : null}
            <span className="flex items-center gap-1">
              <Mail className="size-3.5" /> <StatusBadge status={profile.email_consent} />
            </span>
            <span className="flex items-center gap-1">
              <MessageSquare className="size-3.5" /> <StatusBadge status={profile.sms_consent} />
            </span>
          </span>
        }
        actions={
          <>
            <Button variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirmDelete(true)}>
              Delete
            </Button>
            {!editing ? (
              <Button icon={<Pencil className="size-3.5" />} onClick={() => setEditing(true)}>
                Edit
              </Button>
            ) : null}
          </>
        }
      />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-[360px_minmax(0,1fr)]">
        <div className="space-y-4">
          {editing ? <ProfileEditForm profile={profile} onDone={() => setEditing(false)} /> : <PropertiesCard profile={profile} tz={tz} />}
          <Card title="Lists & segments" padded={false}>
            {lists.length + segments.length === 0 ? (
              <p className="px-4 py-4 text-xs text-zinc-500">Not in any lists or segments.</p>
            ) : (
              <ul className="divide-y divide-zinc-100">
                {lists.map((l) => (
                  <li key={l.id}>
                    <Link to={`/audience/lists/${l.id}`} className="flex items-center gap-2 px-4 py-2 text-sm hover:bg-zinc-50">
                      <ListChecks className="size-3.5 text-zinc-400" /> {l.name}
                    </Link>
                  </li>
                ))}
                {segments.map((s) => (
                  <li key={s.id}>
                    <Link to={`/audience/segments/${s.id}`} className="flex items-center gap-2 px-4 py-2 text-sm hover:bg-zinc-50">
                      <PieChart className="size-3.5 text-zinc-400" /> {s.name}
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        </div>
        <div className="space-y-4">
          <Card title="Activity" padded={false}>
            {events.length === 0 ? (
              <EmptyState icon={<Activity className="size-5" />} title="No activity yet" description="Events like Viewed Product and Placed Order appear here." />
            ) : (
              <ol className="relative px-4 py-3">
                {events.map((e, i) => (
                  <li key={e.id} className="relative flex gap-3 pb-4 last:pb-0">
                    {i < events.length - 1 ? <span className="absolute left-[5px] top-4 h-full w-px bg-zinc-200" /> : null}
                    <span className="relative mt-1.5 size-2.5 shrink-0 rounded-full border-2 border-white bg-accent-500 ring-1 ring-accent-200" />
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-baseline justify-between gap-2">
                        <span className="text-sm font-medium text-zinc-900">{e.metric}</span>
                        <span className="text-xs text-zinc-500" title={formatDateTime(e.occurred_at, tz)}>
                          {formatRelative(e.occurred_at)}
                        </span>
                      </div>
                      {e.value !== null ? <div className="text-xs text-zinc-600">Value: {e.value}</div> : null}
                      {Object.keys(e.properties).length > 0 ? (
                        <details className="mt-0.5">
                          <summary className="cursor-pointer text-xs text-zinc-500 hover:text-zinc-800">{Object.keys(e.properties).length} properties</summary>
                          <pre className="mt-1 max-h-48 overflow-auto rounded-md bg-zinc-50 p-2 font-mono text-[11px] text-zinc-700">{JSON.stringify(e.properties, null, 2)}</pre>
                        </details>
                      ) : null}
                    </div>
                  </li>
                ))}
              </ol>
            )}
          </Card>
          <Card title="Messages received" padded={false}>
            {messages.length === 0 ? (
              <p className="px-4 py-6 text-center text-xs text-zinc-500">No messages sent to this person yet.</p>
            ) : (
              <ul className="divide-y divide-zinc-100">
                {messages.map((m) => {
                  const to = m.campaign_id ? `/campaigns/${m.campaign_id}` : m.flow_id ? `/flows/${m.flow_id}` : null;
                  const body = (
                    <>
                      {m.channel === "email" ? <Mail className="size-4 shrink-0 text-zinc-400" /> : <MessageSquare className="size-4 shrink-0 text-zinc-400" />}
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-sm text-zinc-900">{m.subject || (m.channel === "sms" ? "Text message" : "(no subject)")}</span>
                        <span className="block text-xs text-zinc-500">
                          {m.campaign_id ? "Campaign" : m.flow_id ? "Flow" : "Message"} · {formatDateTime(m.sent_at, tz)}
                        </span>
                      </span>
                      <span className="flex items-center gap-1.5">
                        {m.opened_at ? <MailOpen className="size-3.5 text-accent-600" aria-label="Opened" /> : null}
                        {m.clicked_at ? <MousePointerClick className="size-3.5 text-accent-600" aria-label="Clicked" /> : null}
                        <Badge>{m.status}</Badge>
                      </span>
                    </>
                  );
                  return (
                    <li key={m.id}>
                      {to ? (
                        <Link to={to} className="flex items-center gap-3 px-4 py-2.5 hover:bg-zinc-50">
                          {body}
                        </Link>
                      ) : (
                        <div className="flex items-center gap-3 px-4 py-2.5">{body}</div>
                      )}
                    </li>
                  );
                })}
              </ul>
            )}
          </Card>
        </div>
      </div>
      {confirmDelete ? (
        <ConfirmDialog
          title="Delete this profile?"
          body="Their events and message history are deleted too. This can't be undone."
          confirmLabel="Delete profile"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(profile.id, {
              onSuccess: () => {
                toast.success("Profile deleted");
                navigate("/audience/profiles");
              },
            })
          }
          onClose={() => setConfirmDelete(false)}
        />
      ) : null}
    </Page>
  );
}

function displayValue(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "string") return v;
  return JSON.stringify(v);
}

function PropertiesCard({ profile, tz }: { profile: Profile; tz: string }) {
  const standard: [string, string | null][] = [
    ["First name", profile.first_name],
    ["Last name", profile.last_name],
    ["Email", profile.email],
    ["Phone", profile.phone_number ? formatPhone(profile.phone_number) : null],
    ["External ID", profile.external_id],
    ["Created", formatDateTime(profile.created_at, tz)],
    ["Last active", profile.last_event_at ? formatDateTime(profile.last_event_at, tz) : null],
  ];
  const custom = Object.entries(profile.properties);
  return (
    <Card title="Properties">
      <dl className="space-y-2 text-sm">
        {standard.map(([k, v]) => (
          <div key={k} className="flex justify-between gap-3">
            <dt className="text-zinc-500">{k}</dt>
            <dd className="truncate text-right text-zinc-900">{v ?? "—"}</dd>
          </div>
        ))}
      </dl>
      {custom.length > 0 ? (
        <>
          <div className="mb-2 mt-4 text-[11px] font-semibold uppercase tracking-wider text-zinc-400">Custom</div>
          <dl className="space-y-2 text-sm">
            {custom.map(([k, v]) => (
              <div key={k} className="flex justify-between gap-3">
                <dt className="font-mono text-xs text-zinc-500">{k}</dt>
                <dd className="truncate text-right text-zinc-900" title={displayValue(v)}>
                  {displayValue(v)}
                </dd>
              </div>
            ))}
          </dl>
        </>
      ) : null}
    </Card>
  );
}

/** Text values stay strings; numbers, true/false, null, arrays and objects are parsed as JSON. */
function parseValue(raw: string): unknown {
  const t = raw.trim();
  if (/^(-?\d+(\.\d+)?|true|false|null|\[.*\]|\{.*\})$/s.test(t)) {
    try {
      return JSON.parse(t) as unknown;
    } catch {
      return raw;
    }
  }
  return raw;
}

function ProfileEditForm({ profile, onDone }: { profile: Profile; onDone: () => void }) {
  const update = useUpdateProfile(profile.id);
  const [fields, setFields] = useState({
    first_name: profile.first_name ?? "",
    last_name: profile.last_name ?? "",
    email: profile.email ?? "",
    phone_number: profile.phone_number ?? "",
    external_id: profile.external_id ?? "",
    email_consent: profile.email_consent,
    sms_consent: profile.sms_consent,
  });
  const [props, setProps] = useState(() => Object.entries(profile.properties).map(([key, v]) => ({ key, value: displayValue(v) === "—" ? "" : displayValue(v) })));

  const save = () => {
    const properties: Record<string, unknown> = {};
    for (const p of props) if (p.key.trim()) properties[p.key.trim()] = parseValue(p.value);
    const nullable = (s: string) => s.trim() || null;
    update.mutate(
      {
        first_name: nullable(fields.first_name),
        last_name: nullable(fields.last_name),
        email: nullable(fields.email),
        phone_number: nullable(fields.phone_number),
        external_id: nullable(fields.external_id),
        email_consent: fields.email_consent,
        sms_consent: fields.sms_consent,
        properties,
      },
      {
        onSuccess: () => {
          toast.success("Profile saved");
          onDone();
        },
      },
    );
  };

  const text = (key: "first_name" | "last_name" | "email" | "phone_number" | "external_id", label: string) => (
    <Field label={label}>
      <Input value={fields[key]} onChange={(e) => setFields({ ...fields, [key]: e.target.value })} />
    </Field>
  );

  return (
    <Card title="Edit profile">
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          {text("first_name", "First name")}
          {text("last_name", "Last name")}
        </div>
        {text("email", "Email")}
        {text("phone_number", "Phone")}
        {text("external_id", "External ID")}
        <div className="grid grid-cols-2 gap-3">
          <Field label="Email consent">
            <Select value={fields.email_consent} onChange={(e) => setFields({ ...fields, email_consent: e.target.value as Consent })}>
              <option value="subscribed">Subscribed</option>
              <option value="unsubscribed">Unsubscribed</option>
              <option value="never_subscribed">Never subscribed</option>
              <option value="pending">Waiting to confirm (double opt-in)</option>
            </Select>
          </Field>
          <Field label="SMS consent">
            <Select value={fields.sms_consent} onChange={(e) => setFields({ ...fields, sms_consent: e.target.value as Consent })}>
              <option value="subscribed">Subscribed</option>
              <option value="unsubscribed">Unsubscribed</option>
              <option value="never_subscribed">Never subscribed</option>
            </Select>
          </Field>
        </div>
        <div>
          <div className="mb-1.5 text-xs font-medium text-zinc-700">Custom properties</div>
          <div className="space-y-1.5">
            {props.map((p, i) => (
              <div key={i} className="flex gap-1.5">
                <Input value={p.key} onChange={(e) => setProps(props.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))} placeholder="key" className="w-28 font-mono text-xs" aria-label="Property key" />
                <Input value={p.value} onChange={(e) => setProps(props.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))} placeholder="value" aria-label="Property value" />
                <IconButton label="Remove property" onClick={() => setProps(props.filter((_, j) => j !== i))}>
                  <X className="size-3.5" />
                </IconButton>
              </div>
            ))}
            <Button size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => setProps([...props, { key: "", value: "" }])}>
              Add property
            </Button>
          </div>
        </div>
        <div className="flex justify-end gap-2 border-t border-zinc-100 pt-3">
          <Button variant="ghost" onClick={onDone}>
            Cancel
          </Button>
          <Button variant="primary" loading={update.isPending} onClick={save}>
            Save
          </Button>
        </div>
      </div>
    </Card>
  );
}

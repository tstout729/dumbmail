import { useState, type ChangeEvent, type FormEvent } from "react";
import { AlertTriangle, BellRing, CheckCircle2, Download, KeyRound } from "lucide-react";
import { useSession } from "../../api";
import { Badge, Button, Card, Checkbox, CodeBlock, ConfirmDialog, CopyField, Field, InlineError, Input, Notice, Segmented, Skeleton, Stat, TagInput, Textarea, Toggle } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { useAddTestDevice, useRotateIdentitySecret, useSendTestPush, useUpdatePushSettings, usePushSettings } from "./api";
import type { PushPrompt, PushSettings } from "./types";

/** Settings → Push: web push through the website snippet, Apple and Firebase credentials for apps, and linking devices to people. */
export function PushSettingsPage() {
  const settings = usePushSettings();
  if (settings.isPending) return <Skeleton className="h-64 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  const s = settings.data;
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-3 md:grid-cols-5">
        <Stat label="People reachable" value={formatNumber(s.reachable_people)} sub={s.opted_out ? `${formatNumber(s.opted_out)} turned push off` : undefined} />
        <Stat label="Browsers" value={formatNumber(s.devices.web)} />
        <Stat label="iOS devices" value={formatNumber(s.devices.ios)} />
        <Stat label="Android devices" value={formatNumber(s.devices.android)} />
        <Stat label="Anonymous" value={formatNumber(s.devices.anonymous)} sub="Not signed in on your site or app" />
      </div>
      <WebPush settings={s} />
      <PromptCard key={JSON.stringify(s.web.prompt)} prompt={s.web.prompt} />
      <Apns settings={s} />
      <Fcm settings={s} />
      <Identity settings={s} />
      <Mobile settings={s} />
    </div>
  );
}

const INSTALL: { label: string; steps: string }[] = [
  { label: "Your own server", steps: "Copy the file into the folder your site's root is served from, so it opens at the address below." },
  { label: "Next.js, Vite, React, Astro", steps: "Put the file in your project's public/ folder and deploy." },
  { label: "WordPress", steps: "Upload it with your host's file manager or SFTP into the WordPress folder (the one with wp-config.php)." },
  { label: "Shopify, Wix, Squarespace, Webflow", steps: "These builders don't let you put a file at your site's root, so web push can't be added there. Email, texts and app push still work." },
];

function siteRoot(website: string | null): string {
  const w = (website ?? "").trim();
  if (!w) return "https://yoursite.com";
  return (w.includes("://") ? w : `https://${w}`).replace(/\/+$/, "");
}

function WebPush({ settings }: { settings: PushSettings }) {
  const { account } = useSession();
  const canWrite = useCan("settings.write");
  const update = useUpdatePushSettings();
  const [path, setPath] = useState(settings.web.sw_path);
  const web = settings.web;
  const save = (patch: { web_enabled?: boolean; sw_path?: string }, done: string) => update.mutate(patch, { onSuccess: () => toast.success(done) });
  return (
    <Card
      title="Web push"
      actions={
        <div className="flex items-center gap-2">
          {web.enabled ? <Badge tone="green">On</Badge> : <Badge tone="gray">Off</Badge>}
          <Toggle checked={web.enabled} disabled={!canWrite || update.isPending} onChange={(on) => save({ web_enabled: on }, on ? "Web push is on" : "Web push is off")} label="Web push" />
        </div>
      }
    >
      <div className="space-y-4 text-sm text-zinc-600">
        <p>
          Browsers only show notifications from a small file on your site's own domain, called a service worker. DumbMail can't host it on your domain, so
          you put it there once. Then your website snippet asks visitors with a small card you word yourself, and only opens the browser's prompt when they
          click Allow.
        </p>
        <ol className="list-decimal space-y-3 pl-5">
          <li>
            <div className="flex flex-wrap items-center gap-2">
              <span>Download the service worker file.</span>
              <a className="inline-flex items-center gap-1 text-sm font-medium text-accent-700 hover:underline" href={web.service_worker_url} download="dumbmail-sw.js">
                <Download className="size-3.5" /> dumbmail-sw.js
              </a>
            </div>
          </li>
          <li>
            <span>Put it on your site so it opens at </span>
            <code className="break-all rounded bg-zinc-100 px-1 text-xs">
              {siteRoot(account.website)}
              {web.sw_path}
            </code>
            <ul className="mt-2 space-y-1.5">
              {INSTALL.map((i) => (
                <li key={i.label} className="text-xs">
                  <span className="font-medium text-zinc-800">{i.label}:</span> {i.steps}
                </li>
              ))}
            </ul>
            <form
              className="mt-3 flex flex-wrap items-end gap-2"
              onSubmit={(e: FormEvent) => {
                e.preventDefault();
                save({ sw_path: path }, "Path saved");
              }}
            >
              <Field label="Path on your site">
                <Input value={path} onChange={(e) => setPath(e.target.value)} className="w-56" disabled={!canWrite} />
              </Field>
              {canWrite ? (
                <Button type="submit" variant="secondary" disabled={path === web.sw_path} loading={update.isPending}>
                  Save path
                </Button>
              ) : null}
            </form>
            <p className="mt-3 text-xs">
              DumbMail's worker gets a scope of its own ({web.sw_scope}), so it controls none of your pages and never replaces a service worker your
              site already has. Sharing one worker with your site's own push notifications isn't supported yet.
            </p>
          </li>
          <li>Turn web push on (top right). The website snippet you already have does the rest.</li>
        </ol>
        {update.error ? <InlineError message={update.error.message} /> : null}
        <TestInThisBrowser settings={settings} />
      </div>
    </Card>
  );
}

function urlKey(b64: string): Uint8Array<ArrayBuffer> {
  const s = (b64 + "===".slice((b64.length + 3) % 4)).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(s);
  const out = new Uint8Array(new ArrayBuffer(raw.length));
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

/** Wait until the worker is active. (`serviceWorker.ready` never settles here: the worker's scope doesn't cover this page.) */
async function activated(reg: ServiceWorkerRegistration): Promise<ServiceWorkerRegistration> {
  const worker = reg.installing ?? reg.waiting;
  if (reg.active || !worker) return reg;
  await new Promise<void>((resolve, reject) => {
    const timer = window.setTimeout(() => reject(new Error("The browser didn't start the notification worker.")), 15_000);
    worker.addEventListener("statechange", () => {
      if (worker.state === "activated") {
        window.clearTimeout(timer);
        resolve();
      }
    });
  });
  return reg;
}

/** Subscribe this browser (on the dashboard's own address) as the member's test device, then send it a test. */
function TestInThisBrowser({ settings }: { settings: PushSettings }) {
  const canTest = useCan("campaigns.test");
  const add = useAddTestDevice();
  const send = useSendTestPush();
  const [error, setError] = useState<string | null>(null);
  const supported = typeof window !== "undefined" && window.isSecureContext && "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;
  const key = settings.web.vapid_public_key;
  if (!canTest || !key) return null;
  const run = async () => {
    setError(null);
    try {
      const permission = await Notification.requestPermission();
      if (permission !== "granted") {
        setError(permission === "denied" ? "This browser blocks notifications from DumbMail. Allow them in the browser's site settings and try again." : "Notifications weren't allowed.");
        return;
      }
      const reg = await activated(await navigator.serviceWorker.register(`/push/sw.js?k=${encodeURIComponent(settings.public_key)}`, { scope: "/push/" }));
      const sub = (await reg.pushManager.getSubscription()) ?? (await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: urlKey(key) }));
      await add.mutateAsync(sub.toJSON());
      const r = await send.mutateAsync({ to: "me" });
      const ok = r.devices.filter((d) => d.status === "sent").length;
      if (ok) toast.success("Test sent. It should appear in a moment.");
      else setError(r.devices[0]?.error ?? "The test didn't reach this browser.");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Something went wrong");
    }
  };
  return (
    <div className="rounded-lg bg-zinc-50 p-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <div className="text-sm font-medium text-zinc-900">Send me a test</div>
          <p className="text-xs text-zinc-500">Allows notifications in this browser as your own test device (never part of an audience) and sends one.</p>
        </div>
        <Button variant="secondary" icon={<BellRing className="size-3.5" />} onClick={() => void run()} loading={add.isPending || send.isPending} disabled={!supported}>
          Send me a test
        </Button>
      </div>
      {!supported ? <p className="mt-2 text-xs text-zinc-500">This browser can't receive web push here.</p> : null}
      {error ? <p className="mt-2 text-xs text-red-600">{error}</p> : null}
    </div>
  );
}

function PromptCard({ prompt }: { prompt: PushPrompt }) {
  const canWrite = useCan("settings.write");
  const update = useUpdatePushSettings();
  const [p, setP] = useState(prompt);
  const set = (patch: Partial<PushPrompt>) => setP({ ...p, ...patch });
  const num = (e: ChangeEvent<HTMLInputElement>) => Math.max(0, Math.floor(Number(e.target.value) || 0));
  const dirty = JSON.stringify(p) !== JSON.stringify(prompt);
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        update.mutate({ prompt: p }, { onSuccess: () => toast.success("Prompt saved") });
      }}
    >
      <Card title="Asking for permission">
        <fieldset disabled={!canWrite} className="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_320px]">
          <div className="min-w-0 space-y-3">
            <p className="text-sm text-zinc-600">
              Nobody is asked when a page loads. This card shows first, in your words; only its Allow button opens the browser's own prompt. A browser
              that said no is never asked again.
            </p>
            <Checkbox label="Show the card automatically" description="Off: only your own button asks, with dumbmail.notifications.prompt()." checked={p.enabled} onChange={(e) => set({ enabled: e.target.checked })} />
            <Field label="Title">
              <Input value={p.title} maxLength={80} onChange={(e) => set({ title: e.target.value })} required />
            </Field>
            <Field label="Message">
              <Textarea rows={2} value={p.body} maxLength={300} onChange={(e) => set({ body: e.target.value })} required />
            </Field>
            <div className="grid grid-cols-2 gap-3">
              <Field label="Allow button">
                <Input value={p.allow_text} maxLength={30} onChange={(e) => set({ allow_text: e.target.value })} required />
              </Field>
              <Field label="Dismiss button">
                <Input value={p.deny_text} maxLength={30} onChange={(e) => set({ deny_text: e.target.value })} required />
              </Field>
              <Field label="After pages viewed">
                <Input type="number" min={1} max={50} value={p.page_views} onChange={(e) => set({ page_views: Math.min(50, Math.max(1, num(e))) })} />
              </Field>
              <Field label="After seconds on the page">
                <Input type="number" min={0} max={600} value={p.delay_seconds} onChange={(e) => set({ delay_seconds: Math.min(600, num(e)) })} />
              </Field>
              <Field label="Ask again after (days)" hint="When someone clicks the dismiss button.">
                <Input type="number" min={0} max={365} value={p.frequency_days} onChange={(e) => set({ frequency_days: Math.min(365, num(e)) })} />
              </Field>
              <Field label="Position">
                <Segmented value={p.position} onChange={(position) => set({ position })} options={[{ value: "bottom", label: "Bottom" }, { value: "top", label: "Top" }]} />
              </Field>
            </div>
            <Field label="Only on pages containing" hint="Leave empty for every page.">
              <TagInput value={p.url_contains} onChange={(url_contains) => set({ url_contains })} placeholder="/products/" />
            </Field>
            <Field label="Never on pages containing">
              <TagInput value={p.url_excludes} onChange={(url_excludes) => set({ url_excludes })} placeholder="/checkout" />
            </Field>
            {update.error ? <InlineError message={update.error.message} /> : null}
            {canWrite ? (
              <Button type="submit" variant="primary" disabled={!dirty} loading={update.isPending}>
                Save prompt
              </Button>
            ) : null}
          </div>
          <PromptPreview prompt={p} />
        </fieldset>
      </Card>
    </form>
  );
}

function PromptPreview({ prompt }: { prompt: PushPrompt }) {
  const { account } = useSession();
  return (
    <div className="flex min-h-[220px] flex-col rounded-xl bg-zinc-100 p-4" aria-label="Prompt preview">
      <div className={prompt.position === "top" ? "" : "mt-auto"}>
        <div className="rounded-xl bg-white p-4 shadow-lg">
          <div className="text-[15px] font-semibold text-zinc-900">{prompt.title}</div>
          <div className="mb-3 mt-1 text-sm text-zinc-600">{prompt.body}</div>
          <div className="flex justify-end gap-2">
            <span className="rounded-lg border border-zinc-300 px-3 py-1.5 text-sm font-semibold text-zinc-700">{prompt.deny_text}</span>
            <span className="rounded-lg px-3 py-1.5 text-sm font-semibold text-white" style={{ background: account.brand_color }}>
              {prompt.allow_text}
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}

function Status({ configured, error }: { configured: boolean; error: string | null }) {
  if (error) return <Badge tone="red">Problem</Badge>;
  return configured ? <Badge tone="green">Connected</Badge> : <Badge tone="gray">Not set up</Badge>;
}

function readFile(e: ChangeEvent<HTMLInputElement>, then: (text: string) => void) {
  const f = e.target.files?.[0];
  if (!f) return;
  if (f.size > 20_000) {
    toast.error("That file is too large to be a key file.");
    return;
  }
  void f.text().then(then);
}

function Apns({ settings }: { settings: PushSettings }) {
  const canWrite = useCan("settings.write");
  const update = useUpdatePushSettings();
  const a = settings.apns;
  const [keyId, setKeyId] = useState(a.key_id ?? "");
  const [teamId, setTeamId] = useState(a.team_id ?? "");
  const [bundle, setBundle] = useState(a.bundle_id ?? "");
  const [production, setProduction] = useState(a.production);
  const [p8, setP8] = useState("");
  const [removing, setRemoving] = useState(false);
  return (
    <Card title="Apple push (iOS apps)" actions={<Status configured={a.configured} error={a.error} />}>
      {a.error ? (
        <div className="mb-3">
          <Notice tone="red" icon={<AlertTriangle />} title="Apple refused the last notification">
            {a.error}. Check the key, team ID and bundle ID, and whether this is a development or a production build.
          </Notice>
        </div>
      ) : null}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          update.mutate(
            { apns: { key_id: keyId.trim(), team_id: teamId.trim(), bundle_id: bundle.trim(), production, key_p8: p8.trim() || undefined } },
            {
              onSuccess: () => {
                setP8("");
                toast.success("Apple push saved");
              },
            },
          );
        }}
      >
        <fieldset disabled={!canWrite} className="space-y-3">
          <p className="text-sm text-zinc-600">
            In your Apple developer account, create a key with Apple Push Notifications service (Certificates, Identifiers & Profiles → Keys) and download
            its .p8 file. The key is stored encrypted and never shown again.
          </p>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <Field label="Key ID">
              <Input value={keyId} onChange={(e) => setKeyId(e.target.value)} placeholder="ABC123DEFG" required />
            </Field>
            <Field label="Team ID">
              <Input value={teamId} onChange={(e) => setTeamId(e.target.value)} placeholder="TEAM123456" required />
            </Field>
            <Field label="Bundle ID">
              <Input value={bundle} onChange={(e) => setBundle(e.target.value)} placeholder="com.yourbrand.app" required />
            </Field>
          </div>
          <Field label={a.configured ? "Replace the .p8 key (optional)" : ".p8 key file"}>
            <div className="space-y-2">
              <input type="file" accept=".p8,text/plain" onChange={(e) => readFile(e, setP8)} className="block text-sm text-zinc-600" aria-label=".p8 key file" />
              {p8 ? (
                <span className="flex items-center gap-1 text-xs text-accent-700">
                  <CheckCircle2 className="size-3.5" /> Key file read
                </span>
              ) : null}
            </div>
          </Field>
          <Checkbox label="Production" description="Turn off for development builds from Xcode (they use Apple's sandbox)." checked={production} onChange={(e) => setProduction(e.target.checked)} />
          {update.error ? <InlineError message={update.error.message} /> : null}
          {canWrite ? (
            <div className="flex flex-wrap gap-2">
              <Button type="submit" variant="primary" loading={update.isPending} disabled={!a.configured && !p8}>
                Save
              </Button>
              {a.configured ? (
                <Button variant="ghost" onClick={() => setRemoving(true)}>
                  Remove
                </Button>
              ) : null}
            </div>
          ) : null}
        </fieldset>
      </form>
      {removing ? (
        <ConfirmDialog
          title="Remove Apple push?"
          body="iOS devices stop getting notifications until you add a key again."
          confirmLabel="Remove"
          danger
          loading={update.isPending}
          onConfirm={() => update.mutate({ apns: null }, { onSuccess: () => setRemoving(false) })}
          onClose={() => setRemoving(false)}
        />
      ) : null}
    </Card>
  );
}

function Fcm({ settings }: { settings: PushSettings }) {
  const canWrite = useCan("settings.write");
  const update = useUpdatePushSettings();
  const f = settings.fcm;
  const [json, setJson] = useState("");
  const [removing, setRemoving] = useState(false);
  return (
    <Card title="Firebase (Android apps)" actions={<Status configured={f.configured} error={f.error} />}>
      {f.error ? (
        <div className="mb-3">
          <Notice tone="red" icon={<AlertTriangle />} title="Firebase refused the last notification">
            {f.error}. Check that the service account belongs to the app's Firebase project.
          </Notice>
        </div>
      ) : null}
      {f.configured ? (
        <p className="mb-3 text-sm text-zinc-600">
          Project <b>{f.project_id}</b>, service account {f.client_email}.
        </p>
      ) : null}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          update.mutate(
            { fcm: { service_account_json: json } },
            {
              onSuccess: () => {
                setJson("");
                toast.success("Firebase saved");
              },
            },
          );
        }}
      >
        <fieldset disabled={!canWrite} className="space-y-3">
          <p className="text-sm text-zinc-600">
            In Firebase, open Project settings → Service accounts → Generate new private key, and add the file here. It's stored encrypted and never shown
            again.
          </p>
          <div className="flex flex-wrap items-center gap-3">
            <input type="file" accept=".json,application/json" onChange={(e) => readFile(e, setJson)} className="block text-sm text-zinc-600" aria-label="Service account file" />
            {json ? (
              <span className="flex items-center gap-1 text-xs text-accent-700">
                <CheckCircle2 className="size-3.5" /> File read
              </span>
            ) : null}
          </div>
          {update.error ? <InlineError message={update.error.message} /> : null}
          {canWrite ? (
            <div className="flex flex-wrap gap-2">
              <Button type="submit" variant="primary" loading={update.isPending} disabled={!json}>
                {f.configured ? "Replace" : "Save"}
              </Button>
              {f.configured ? (
                <Button variant="ghost" onClick={() => setRemoving(true)}>
                  Remove
                </Button>
              ) : null}
            </div>
          ) : null}
        </fieldset>
      </form>
      {removing ? (
        <ConfirmDialog
          title="Remove Firebase?"
          body="Android devices stop getting notifications until you add a service account again."
          confirmLabel="Remove"
          danger
          loading={update.isPending}
          onConfirm={() => update.mutate({ fcm: null }, { onSuccess: () => setRemoving(false) })}
          onClose={() => setRemoving(false)}
        />
      ) : null}
    </Card>
  );
}

function Identity({ settings }: { settings: PushSettings }) {
  const canWrite = useCan("settings.write");
  const rotate = useRotateIdentitySecret();
  const [confirm, setConfirm] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);
  return (
    <Card title="Linking devices to people" actions={settings.identity.configured ? <Badge tone="green">Secret {settings.identity.hint}</Badge> : null}>
      <div className="space-y-3 text-sm text-zinc-600">
        <p>
          Anyone can copy your site key, and email links get forwarded, so neither can say who a device belongs to. A device is linked to a person only with an
          enrollment grant your own server makes for whoever is signed in, valid for at most a day. Until then it gets an anonymous profile of its own.
        </p>
        <Field label="Your account id (part of every grant)">
          <CopyField value={settings.account_id} />
        </Field>
        <CodeBlock
          label="On your server (Node.js; Python, PHP and Ruby are in the push guide)"
          code={`const expires = Math.floor(Date.now() / 1000) + 3600;
const mac = require("crypto").createHmac("sha256", process.env.DUMBMAIL_PUSH_SECRET)
  .update(\`v1|push-enroll|${settings.account_id}|external_id|\${customer.id}|\${expires}\`).digest("hex");
const grant = \`v1.\${expires}.\${mac}\`;`}
        />
        <CodeBlock
          label="On your site, after sign-in (and on sign-out)"
          code={`dumbmail.notifications.identify({ type: "external_id", value: customerId, grant });
dumbmail.notifications.signOut();`}
        />
        {secret ? (
          <Notice tone="amber" icon={<KeyRound />} title="Copy this secret now. It won't be shown again.">
            <div className="mt-2">
              <CopyField value={secret} />
            </div>
          </Notice>
        ) : null}
        {canWrite ? (
          <Button variant="secondary" icon={<KeyRound className="size-3.5" />} onClick={() => setConfirm(true)}>
            {settings.identity.configured ? "Make a new secret" : "Make a secret"}
          </Button>
        ) : null}
        {rotate.error ? <InlineError message={rotate.error.message} /> : null}
      </div>
      {confirm ? (
        <ConfirmDialog
          title={settings.identity.configured ? "Replace the secret?" : "Make a secret?"}
          body={settings.identity.configured ? "Your server must switch to the new one: grants made with the old secret stop working right away." : "You'll see it once. Keep it on your server only."}
          confirmLabel="Make secret"
          loading={rotate.isPending}
          onConfirm={() =>
            rotate.mutate(undefined, {
              onSuccess: (r) => {
                setSecret(r.identity_secret);
                setConfirm(false);
              },
            })
          }
          onClose={() => setConfirm(false)}
        />
      ) : null}
    </Card>
  );
}

function Mobile({ settings }: { settings: PushSettings }) {
  return (
    <Card title="Your app's code">
      <div className="space-y-3 text-sm text-zinc-600">
        <p>
          There's no DumbMail app SDK to install. Your app asks for permission, then sends its device token and the permission it got to this address on every
          launch, after sign-in and whenever the permission changes, and reports when someone opens a notification. Only devices that allowed notifications
          are sent to. The full Swift, Kotlin and React Native code is in the push guide (the MCP resource dumbmail://docs/push, or docs/PUSH.md).
        </p>
        <Field label="Register a device (POST, JSON)">
          <CopyField value={settings.register_url} />
        </Field>
        <CodeBlock
          label="Body"
          code={`{"platform": "ios" | "android", "token": "<device token>", "permission": "granted" | "provisional" | "denied" | "not_determined",
 "identity": {"type": "external_id", "value": "<customer id>", "grant": "<from your server>"}}`}
        />
      </div>
    </Card>
  );
}

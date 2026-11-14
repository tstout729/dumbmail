import { useState, type FormEvent, type ReactNode } from "react";
import { Link } from "react-router";
import { CheckCircle2, CircleDashed, Search, XCircle } from "lucide-react";
import type { Form, WebsiteStatus } from "../../api";
import { useCheckWebsite, useForms, useSession, useWebsite } from "../../api";
import { Button, Card, CodeBlock, CopyField, Field, Input, Notice, Select, Skeleton, Tabs } from "../../components/ui";
import { formatRelative } from "../../lib/format";

export function WebsitePage() {
  const website = useWebsite();
  if (website.isPending) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-48" />
        <Skeleton className="h-72" />
      </div>
    );
  }
  if (website.error) return <p className="text-sm text-red-600">{website.error.message}</p>;
  const w = website.data;
  return (
    <div className="space-y-4">
      <SnippetCard w={w} />
      <GuidesCard snippet={w.snippet} />
      <EventsCard />
      <EmbedCard />
    </div>
  );
}

function SnippetCard({ w }: { w: WebsiteStatus }) {
  const { account } = useSession();
  const check = useCheckWebsite();
  const result = check.data;

  const onCheck = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    let url = String(new FormData(e.currentTarget).get("url")).trim();
    if (url && !/^https?:\/\//i.test(url)) url = `https://${url}`;
    check.mutate(url);
  };

  return (
    <Card title="Website snippet">
      <div className="space-y-4">
        <p className="text-sm text-zinc-600">
          Add this once to every page, in the <code className="rounded bg-zinc-100 px-1 text-xs">&lt;head&gt;</code> or footer. It tracks visits,
          recognizes people who arrive from your emails and texts, shows your popup and flyout forms, and renders embedded forms. It uses your
          public key, which is safe to expose.
        </p>
        <CodeBlock code={w.snippet} label="HTML" />
        {w.installed ? (
          <Notice tone="green" icon={<CheckCircle2 />} title="Installed">
            Last seen on{" "}
            {w.last_seen_url ? (
              <a href={w.last_seen_url} target="_blank" rel="noreferrer" className="break-all font-medium underline">
                {w.last_seen_url}
              </a>
            ) : (
              "your site"
            )}{" "}
            {formatRelative(w.last_seen_at)}.
          </Notice>
        ) : (
          <Notice tone="gray" icon={<CircleDashed />} title="Not detected yet">
            It shows as installed once someone loads a page with the snippet. Check your site below to test it now.
          </Notice>
        )}
        <form onSubmit={onCheck} className="flex flex-wrap items-end gap-2">
          <Field label="Page to check" className="min-w-0 flex-1">
            <Input name="url" required defaultValue={account.website ?? ""} placeholder="https://yourstore.com" />
          </Field>
          <Button type="submit" icon={<Search className="size-3.5" />} loading={check.isPending}>
            Check my site
          </Button>
        </form>
        {result ? (
          <Notice tone={result.found ? "green" : "amber"} icon={result.found ? <CheckCircle2 /> : <XCircle />} title={result.found ? "Snippet found" : "Snippet not found"}>
            {result.message}
          </Notice>
        ) : null}
        <Field label="Public key">
          <CopyField value={w.public_key} />
        </Field>
      </div>
    </Card>
  );
}

type GuideKey = "shopify" | "wordpress" | "squarespace" | "wix" | "webflow" | "gtm" | "framer" | "nextjs" | "html";
type Guide = { label: string; steps: ReactNode[]; code?: (src: string, snippet: string) => { code: string; label: string }; note?: ReactNode };

const c = (s: string) => <code className="rounded bg-zinc-100 px-1 text-xs">{s}</code>;
const b = (s: string) => <strong className="font-semibold text-zinc-900">{s}</strong>;

const guides: Record<GuideKey, Guide> = {
  shopify: {
    label: "Shopify",
    steps: [
      <>In Shopify admin, go to {b("Online Store → Themes")}.</>,
      <>Next to your current theme, click {b("…")} → {b("Edit code")}.</>,
      <>Open {c("layout/theme.liquid")}.</>,
      <>Paste the snippet on the line just before {c("</head>")} and click {b("Save")}.</>,
    ],
  },
  wordpress: {
    label: "WordPress",
    steps: [
      <>Install a header-code plugin: {b("WPCode")} or {b("Insert Headers and Footers")} (Plugins → Add New).</>,
      <>Open the plugin's header and footer settings (WPCode: {b("Code Snippets → Header & Footer")}).</>,
      <>Paste the snippet into the {b("Header")} box and save.</>,
    ],
  },
  squarespace: {
    label: "Squarespace",
    steps: [
      <>Open {b("Settings → Developer tools → Code injection")}.</>,
      <>Paste the snippet into {b("Header")} and click {b("Save")}.</>,
    ],
  },
  wix: {
    label: "Wix",
    steps: [
      <>In your site dashboard, open {b("Settings → Custom code")}.</>,
      <>Click {b("Add code")} and paste the snippet.</>,
      <>Choose {b("All pages")} (load once), place the code in {b("Head")}, and click {b("Apply")}.</>,
    ],
  },
  webflow: {
    label: "Webflow",
    steps: [
      <>Open {b("Site settings → Custom code")}.</>,
      <>Paste the snippet into {b("Head code")} and save.</>,
      <>{b("Publish")} the site; custom code only runs on the published site.</>,
    ],
  },
  gtm: {
    label: "Google Tag Manager",
    steps: [
      <>In your container, go to {b("Tags → New")}.</>,
      <>Tag configuration: {b("Custom HTML")}. Paste the snippet.</>,
      <>Triggering: {b("All Pages")}. Save, then {b("Submit")} and publish the container.</>,
    ],
  },
  framer: {
    label: "Framer",
    steps: [
      <>Open {b("Site settings → General → Custom code")}.</>,
      <>Paste the snippet into {b("Start of <head> tag")} and save.</>,
      <>{b("Publish")} your site.</>,
    ],
  },
  nextjs: {
    label: "Next.js / React",
    steps: [
      <>Add the script to your root layout: {c("app/layout.tsx")} (App Router) or {c("pages/_app.tsx")} (Pages Router).</>,
      <>Deploy.</>,
    ],
    code: (src) => ({
      label: "app/layout.tsx",
      code: `import Script from "next/script";

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>
        {children}
        <Script src="${src}" strategy="afterInteractive" />
      </body>
    </html>
  );
}`,
    }),
    note: <>Plain React (Vite, Create React App): paste the HTML snippet into the {c("<head>")} of {c("index.html")}.</>,
  },
  html: {
    label: "HTML",
    steps: [
      <>Paste the snippet into the {c("<head>")} of every page, or into the header include your pages share.</>,
      <>Upload the changed files.</>,
    ],
  },
};

const guideKeys = Object.keys(guides) as GuideKey[];

function scriptSrc(snippet: string): string {
  return /src="([^"]+)"/.exec(snippet)?.[1] ?? "";
}

function GuidesCard({ snippet }: { snippet: string }) {
  const [tab, setTab] = useState<GuideKey>("shopify");
  const guide = guides[tab];
  const code = guide.code ? guide.code(scriptSrc(snippet), snippet) : { code: snippet, label: "HTML" };
  return (
    <Card title="How to install">
      <Tabs value={tab} onChange={setTab} tabs={guideKeys.map((k) => ({ value: k, label: guides[k].label }))} />
      <ol className="mb-4 list-decimal space-y-1.5 pl-5 text-sm text-zinc-700 marker:text-zinc-400">
        {guide.steps.map((step, i) => (
          <li key={i}>{step}</li>
        ))}
      </ol>
      <CodeBlock code={code.code} label={code.label} />
      {guide.note ? <p className="mt-3 text-xs text-zinc-500">{guide.note}</p> : null}
    </Card>
  );
}

const queueStub = `<script>
  // Lets you call postlark before the snippet has loaded; queued calls run once it does.
  window.postlark = window.postlark || { q: [] };
  ["identify", "track"].forEach(function (m) {
    window.postlark[m] = window.postlark[m] || function () {
      window.postlark.q.push([m].concat([].slice.call(arguments)));
    };
  });
</script>`;

const eventsExample = `// When someone logs in, checks out or gives you their email
postlark.identify({ email: "jane@example.com" });

// On a product page: metric, properties, value
postlark.track("Viewed Product", {
  name: "Ceramic Mug",
  price: 18,
  url: "https://yourstore.com/products/ceramic-mug",
  image_url: "https://yourstore.com/images/ceramic-mug.jpg"
}, 18);`;

function EventsCard() {
  return (
    <Card title="Track events from your site">
      <p className="mb-3 text-sm text-zinc-600">
        The snippet tracks {c("Active on Site")} on its own. Call {c("postlark.track")} for anything else, like {c("Viewed Product")} for
        browse-abandonment flows. Events are recorded for people the browser knows: anyone who signed up through a form, arrived from a tracked
        email or text, or was passed to {c("postlark.identify")}.
      </p>
      <CodeBlock code={eventsExample} label="JavaScript" />
      <p className="mb-2 mt-4 text-xs text-zinc-500">Calling it from code that may run before the snippet loads? Add this first:</p>
      <CodeBlock code={queueStub} label="HTML" />
    </Card>
  );
}

function embedForms(forms: Form[]): Form[] {
  return [...forms].sort((a, z) => Number(z.kind === "embed") - Number(a.kind === "embed"));
}

function EmbedCard() {
  const forms = useForms();
  const options = embedForms(forms.data ?? []).filter((f) => f.kind !== "page");
  const [picked, setPicked] = useState<string | null>(null);
  const formId = picked ?? options[0]?.id ?? "FORM_ID";
  return (
    <Card title="Embed a form">
      <p className="mb-3 text-sm text-zinc-600">
        Put this where an embedded form should appear. With the snippet installed, the form renders inside it. Popups and flyouts need no extra
        code; they show on their own using each form's targeting rules.
      </p>
      {options.length > 0 ? (
        <Field label="Form" className="mb-3 max-w-xs">
          <Select value={formId} onChange={(e) => setPicked(e.target.value)}>
            {options.map((f) => (
              <option key={f.id} value={f.id}>
                {f.name}
                {f.kind !== "embed" ? ` (${f.kind})` : ""}
                {f.status !== "live" ? " · draft" : ""}
              </option>
            ))}
          </Select>
        </Field>
      ) : (
        <p className="mb-3 text-xs text-zinc-500">
          <Link to="/forms?create=1" className="font-medium text-accent-700 hover:underline">
            Create an embedded form
          </Link>{" "}
          and its ID fills in here.
        </p>
      )}
      <CodeBlock code={`<div data-postlark-form="${formId}"></div>`} label="HTML" />
    </Card>
  );
}

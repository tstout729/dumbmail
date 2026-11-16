import { useState, type FormEvent, type ReactNode } from "react";
import { Link } from "react-router";
import { AlertTriangle, CheckCircle2, CircleDashed, ExternalLink, FlaskConical, Search, XCircle } from "lucide-react";
import type { Form, SitePlatform, SiteTest, SiteTestCheck, SiteTestCheckKey, WebsiteStatus } from "../../api";
import { useCheckWebsite, useForms, useSession, useSiteTest, useStartSiteTest, useWebsite } from "../../api";
import { Button, buttonClass, Card, CodeBlock, CopyButton, CopyField, Field, Input, Notice, Select, Skeleton, Tabs, cx } from "../../components/ui";
import { formatRelative } from "../../lib/format";

/** Only http(s) URLs become links; anything else is shown as plain text. */
function safeHttpUrl(u: string | null | undefined): string | null {
  if (!u) return null;
  try {
    const parsed = new URL(u);
    return parsed.protocol === "https:" || parsed.protocol === "http:" ? parsed.href : null;
  } catch {
    return null;
  }
}

function hostOf(u: string | null | undefined): string {
  try {
    return u ? new URL(u).host : "";
  } catch {
    return "";
  }
}

type GuideKey = "shopify" | "wordpress" | "squarespace" | "wix" | "webflow" | "gtm" | "framer" | "nextjs" | "html";

const platformGuide: Record<SitePlatform, GuideKey> = {
  shopify: "shopify",
  wordpress: "wordpress",
  squarespace: "squarespace",
  wix: "wix",
  webflow: "webflow",
  framer: "framer",
  nextjs: "nextjs",
  bigcommerce: "html",
};

const platformNames: Record<string, string> = {
  shopify: "Shopify",
  woocommerce: "WooCommerce",
  wordpress: "WordPress",
  squarespace: "Squarespace",
  wix: "Wix",
  webflow: "Webflow",
  framer: "Framer",
  nextjs: "Next.js",
  bigcommerce: "BigCommerce",
};

export function WebsitePage() {
  const website = useWebsite();
  const [guide, setGuide] = useState<GuideKey>("shopify");
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
      <SnippetCard w={w} onPlatform={(p) => setGuide(platformGuide[p])} />
      <TestCard onShowGuide={setGuide} />
      <GuidesCard w={w} tab={guide} onTab={setGuide} />
      <EventsCard />
      <EmbedCard />
    </div>
  );
}

function SnippetCard({ w, onPlatform }: { w: WebsiteStatus; onPlatform: (p: SitePlatform) => void }) {
  const { account } = useSession();
  const check = useCheckWebsite();
  const result = check.data;

  const onCheck = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    let url = String(new FormData(e.currentTarget).get("url")).trim();
    if (url && !/^https?:\/\//i.test(url)) url = `https://${url}`;
    check.mutate(url, { onSuccess: (r) => (r.platform ? onPlatform(r.platform) : undefined) });
  };

  return (
    <Card title="Website snippet">
      <div className="space-y-4">
        <p className="text-sm text-zinc-600">
          Add this once to every page, in the <code className="rounded bg-zinc-100 px-1 text-xs">&lt;head&gt;</code> or footer. It tracks visits,
          product views, carts, checkouts and orders, recognizes your customers, and shows your sign-up forms. It uses your public key, which is
          safe to expose.
        </p>
        <CodeBlock code={w.snippet} label="HTML" />
        {w.installed ? (
          <Notice tone="green" icon={<CheckCircle2 />} title="Installed">
            Last seen on{" "}
            {safeHttpUrl(w.last_seen_url) ? (
              <a href={safeHttpUrl(w.last_seen_url) ?? undefined} target="_blank" rel="noreferrer" className="break-all font-medium underline">
                {w.last_seen_url}
              </a>
            ) : (
              <span className="break-all">{w.last_seen_url ?? "your site"}</span>
            )}{" "}
            {formatRelative(w.last_seen_at)}.
          </Notice>
        ) : (
          <Notice tone="gray" icon={<CircleDashed />} title="Not detected yet">
            It shows as installed once someone loads a page with the snippet. Check your site below, or run the setup test.
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
            {result.platform ? ` Your site looks like ${platformNames[result.platform]}; its install guide is open below.` : null}
          </Notice>
        ) : null}
        <Field label="Public key">
          <CopyField value={w.public_key} />
        </Field>
      </div>
    </Card>
  );
}

// ---------- Test your setup ----------

type TestStep = { key: SiteTestCheckKey; title: string; hint: ReactNode; optional?: boolean; store?: boolean };

function testSteps(platform: string | undefined, onShowGuide: (g: GuideKey) => void): TestStep[] {
  const shopify = platform === "shopify";
  const pixelLink = (
    <button type="button" className="font-medium text-accent-700 hover:underline" onClick={() => onShowGuide("shopify")}>
      Shopify install guide
    </button>
  );
  const steps: TestStep[] = [
    { key: "loaded", title: "Script running on your site", hint: "Open your site with the button above." },
    { key: "forms", title: "Sign-up forms load", hint: "Checked as soon as the script runs." },
    { key: "product", title: "Product views", hint: "Open any product page.", store: true },
    { key: "cart", title: "Add to cart", hint: "Add a product to your cart.", store: true },
  ];
  if (shopify) {
    steps.push({ key: "pixel", title: "Shopify checkout pixel", hint: <>Go to checkout. If this stays gray, add the checkout pixel from the {pixelLink}.</>, store: true });
  }
  steps.push(
    { key: "checkout", title: "Checkout", hint: shopify ? "Go to checkout and enter an email address." : "Go to checkout.", store: true },
    { key: "identify", title: "Visitor recognition", hint: "Enter your email in the checkout email box, or submit a sign-up or contact form." },
    {
      key: "order",
      title: "Orders",
      hint: shopify ? "Optional: place a test order (Shopify's test payments or Bogus Gateway)." : "Optional: place a test order.",
      optional: true,
      store: true,
    },
  );
  return steps;
}

function TestCard({ onShowGuide }: { onShowGuide: (g: GuideKey) => void }) {
  const { account } = useSession();
  const start = useStartSiteTest();
  const [code, setCode] = useState<string | null>(null);
  const test = useSiteTest(code);
  const t = test.data;

  const onStart = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const url = String(new FormData(e.currentTarget).get("url")).trim();
    start.mutate(url, { onSuccess: (r) => setCode(r.code) });
  };

  return (
    <Card title="Test your setup" actions={t ? <Button size="sm" variant="ghost" onClick={() => setCode(null)}>New test</Button> : undefined}>
      {!t ? (
        <div className="space-y-4">
          <p className="text-sm text-zinc-600">
            Open your live site in test mode. A small DumbMail panel appears on your site, and each step here turns green as you click through it.
            Nothing you do in test mode is saved as real activity, so you can even place a test order.
          </p>
          <form onSubmit={onStart} className="flex flex-wrap items-end gap-2">
            <Field label="Your site" className="min-w-0 flex-1">
              <Input name="url" defaultValue={account.website ?? ""} placeholder="https://yourstore.com" />
            </Field>
            <Button type="submit" variant="primary" icon={<FlaskConical className="size-3.5" />} loading={start.isPending}>
              Start test
            </Button>
          </form>
          {start.error ? <p className="text-sm text-red-600">{start.error.message}</p> : null}
        </div>
      ) : (
        <TestResults t={t} onShowGuide={onShowGuide} />
      )}
    </Card>
  );
}

function TestResults({ t, onShowGuide }: { t: SiteTest; onShowGuide: (g: GuideKey) => void }) {
  const checks = t.checks;
  const platform = str(checks.loaded?.platform) ?? undefined;
  const steps = testSteps(platform, onShowGuide);
  const required = steps.filter((s) => !s.optional);
  const passed = required.filter((s) => checks[s.key]).length;
  const warnings = (["warn_duplicate", "warn_csp", "warn_key", "warn_network"] as const).map((k) => str(checks[k]?.message)).filter((m): m is string => m !== null);
  const openUrl = safeHttpUrl(t.open_url);

  return (
    <div className="space-y-4">
      {t.active ? (
        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            {openUrl ? (
              <a href={openUrl} target="_blank" rel="noopener" className={buttonClass("primary")}>
                <ExternalLink className="size-3.5" />
                Open {hostOf(openUrl)} in test mode
              </a>
            ) : null}
            {openUrl ? <CopyButton text={openUrl} label="Copy link" /> : null}
          </div>
          <p className="text-xs text-zinc-500">
            Use it on your phone too: open the copied link there. Any page works if you add{" "}
            <code className="rounded bg-zinc-100 px-1">?dumbmail_test={t.code}</code> to its address. Test mode ends{" "}
            {formatRelative(t.expires_at)}.
          </p>
        </div>
      ) : (
        <Notice tone="gray" icon={<CircleDashed />} title="This test has ended">
          Start a new test to check again.
        </Notice>
      )}

      {checks.wrong_key ? (
        <Notice tone="red" icon={<XCircle />} title="Your site has a different DumbMail snippet">
          The snippet on your site uses the key {str(checks.wrong_key.key) ?? "of another account"}…, which isn't this account&apos;s. Replace it with the snippet above.
        </Notice>
      ) : null}
      {warnings.map((m) => (
        <Notice key={m} tone="amber" icon={<AlertTriangle />} title="Fix this on your site">
          {m}
        </Notice>
      ))}

      <div>
        <div className="mb-2 flex items-center justify-between text-sm">
          <span className="font-medium text-zinc-900">
            {passed} of {required.length} checks passed
          </span>
          {t.last_seen_at ? <span className="text-xs text-zinc-500">Last report {formatRelative(t.last_seen_at)}</span> : null}
        </div>
        <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
          {steps.map((s) => (
            <TestRow key={s.key} step={s} check={checks[s.key]} />
          ))}
          {checks.custom ? <TestRow step={{ key: "custom", title: "Your own events", hint: "" }} check={checks.custom} /> : null}
        </ul>
        <p className="mt-2 text-xs text-zinc-500">
          Not a store? Only the script, forms and visitor recognition matter; the shopping steps stay gray.
          {platform ? ` Detected: ${platformNames[platform] ?? "a custom site"}.` : null}
        </p>
      </div>

      {!checks.loaded && t.active ? (
        <Notice tone="gray" icon={<CircleDashed />} title="Waiting for your site">
          <ul className="mt-1 list-disc space-y-0.5 pl-4">
            <li>Make sure the snippet is saved and your site is published.</li>
            <li>Clear your site&apos;s cache if it uses a caching plugin or CDN.</li>
            <li>If your site redirects and drops the test code, add ?dumbmail_test={t.code} to the address again.</li>
            <li>Ad blockers can block the script; try a private window.</li>
          </ul>
        </Notice>
      ) : null}
    </div>
  );
}

/** Reports come from the customer's site, so show only text that really is text. */
function str(v: unknown): string | null {
  return typeof v === "string" && v ? v : null;
}

function TestRow({ step, check }: { step: TestStep; check: SiteTestCheck | undefined }) {
  const done = !!check;
  const by = str(check?.tracked_by);
  return (
    <li className="flex items-start gap-3 px-3.5 py-2.5">
      <span className={cx("mt-0.5 shrink-0 [&>svg]:size-4", done ? "text-accent-700" : "text-zinc-300")}>{done ? <CheckCircle2 /> : <CircleDashed />}</span>
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium text-zinc-900">
          {step.title}
          {step.optional ? <span className="ml-1.5 text-xs font-normal text-zinc-400">optional</span> : null}
        </div>
        <div className="text-xs break-words text-zinc-500">{check ? (str(check.summary) ?? "Working") : step.hint}</div>
      </div>
      {by ? <span className="shrink-0 text-xs text-zinc-400">{trackedBy[by] ?? by}</span> : null}
    </li>
  );
}

const trackedBy: Record<string, string> = {
  shopify: "Shopify",
  shopify_pixel: "checkout pixel",
  woocommerce: "WooCommerce",
  structured_data: "product data",
  microdata: "product data",
  open_graph: "page tags",
  data_layer: "Google Analytics events",
  network: "cart request",
  form: "cart form",
  click: "button click",
  page: "page content",
  checkout_button: "checkout button",
  manual: "your code",
};

// ---------- Install guides ----------

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
    label: "WordPress / WooCommerce",
    steps: [
      <>Install a header-code plugin: {b("WPCode")} or {b("Insert Headers and Footers")} (Plugins → Add New).</>,
      <>Open the plugin's header and footer settings (WPCode: {b("Code Snippets → Header & Footer")}).</>,
      <>Paste the snippet into the {b("Header")} box and save.</>,
    ],
    note: <>WooCommerce product views, carts, checkout and orders are tracked automatically; no extra code.</>,
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
    note: <>Already sending Google Analytics ecommerce events (view_item, add_to_cart, begin_checkout, purchase)? DumbMail picks those up too.</>,
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
    note: <>Plain React (Vite, Create React App): paste the HTML snippet into the {c("<head>")} of {c("index.html")}. Page changes in single-page apps are followed automatically.</>,
  },
  html: {
    label: "HTML / other",
    steps: [
      <>Paste the snippet into the {c("<head>")} of every page, or into the header include your pages share.</>,
      <>Upload the changed files.</>,
    ],
    note: <>BigCommerce: {b("Storefront → Script Manager → Create a Script")}, location {b("Head")}, {b("All pages")}.</>,
  },
};

const guideKeys = Object.keys(guides) as GuideKey[];

function scriptSrc(snippet: string): string {
  return /src="([^"]+)"/.exec(snippet)?.[1] ?? "";
}

function GuidesCard({ w, tab, onTab }: { w: WebsiteStatus; tab: GuideKey; onTab: (g: GuideKey) => void }) {
  const guide = guides[tab];
  const code = guide.code ? guide.code(scriptSrc(w.snippet), w.snippet) : { code: w.snippet, label: "HTML" };
  return (
    <Card title="How to install">
      <Tabs value={tab} onChange={onTab} tabs={guideKeys.map((k) => ({ value: k, label: guides[k].label }))} />
      <ol className="mb-4 list-decimal space-y-1.5 pl-5 text-sm text-zinc-700 marker:text-zinc-400">
        {guide.steps.map((step, i) => (
          <li key={i}>{step}</li>
        ))}
      </ol>
      <CodeBlock code={code.code} label={code.label} />
      {guide.note ? <p className="mt-3 text-xs text-zinc-500">{guide.note}</p> : null}
      {tab === "shopify" ? <ShopifyPixel pixel={w.shopify_pixel} /> : null}
    </Card>
  );
}

function ShopifyPixel({ pixel }: { pixel: string }) {
  return (
    <div className="mt-6 border-t border-zinc-100 pt-5">
      <h3 className="mb-1 text-sm font-semibold text-zinc-900">Then add checkout tracking</h3>
      <p className="mb-3 text-sm text-zinc-600">
        Shopify doesn&apos;t run theme code on checkout pages, so this pixel reports checkouts (with the email typed there) and orders. Without it,
        checkout and order reminders won&apos;t start on Shopify.
      </p>
      <ol className="mb-4 list-decimal space-y-1.5 pl-5 text-sm text-zinc-700 marker:text-zinc-400">
        <li>
          In Shopify admin, go to {b("Settings → Customer events")} and click {b("Add custom pixel")}. Name it {b("DumbMail")}.
        </li>
        <li>
          Under {b("Customer privacy")}, set Permission to {b("Marketing")} and Data sale to {b("Data collected does not qualify as data sale")}.
        </li>
        <li>Delete the sample code, paste this, then click {b("Save")} and {b("Connect")}.</li>
      </ol>
      <CodeBlock code={pixel} label="Custom pixel" />
    </div>
  );
}

// ---------- Events ----------

const customExample = `// Recognize someone (after login, or when they give you their email)
dumbmail.identify({ email: "jane@example.com" });

// Shopping events, if your site isn't tracked automatically
dumbmail.viewedProduct({ name: "Ceramic Mug", price: 18, image_url: "https://yourstore.com/mug.jpg", url: "https://yourstore.com/mug" });
dumbmail.addedToCart({ name: "Ceramic Mug", price: 18, quantity: 1 });
dumbmail.startedCheckout({ items: [{ name: "Ceramic Mug", price: 18, quantity: 1 }], total: 18 });
dumbmail.placedOrder({ order_id: "1001", total: 18, email: "jane@example.com", items: [{ name: "Ceramic Mug", price: 18, quantity: 1 }] });

// Anything else: metric, properties, value
dumbmail.track("Booked Demo", { plan: "Pro" });`;

const queueStub = `<script>
  // Lets you call dumbmail before the snippet has loaded; queued calls run once it does.
  window.dumbmail = window.dumbmail || { q: [] };
  ["identify", "track", "viewedProduct", "addedToCart", "startedCheckout", "placedOrder"].forEach(function (m) {
    window.dumbmail[m] = window.dumbmail[m] || function () {
      window.dumbmail.q.push([m].concat([].slice.call(arguments)));
    };
  });
</script>`;

const optOut = `<script>
  // Before the snippet: turn off automatic shopping events or email recognition.
  window.dumbmailConfig = { autoTrack: false, captureEmails: false };
</script>`;

function EventsCard() {
  return (
    <Card title="What it tracks">
      <ul className="mb-4 space-y-2 text-sm text-zinc-700">
        <li>
          {b("Active on Site")} on every visit.
        </li>
        <li>
          {b("Viewed Product, Added to Cart, Started Checkout and Placed Order")} on their own: on Shopify (checkout and orders through the checkout
          pixel), WooCommerce, any site whose product pages include product data (most store builders add it), and any site sending Google
          Analytics ecommerce events. These start your browse, cart and checkout reminder flows.
        </li>
        <li>
          {b("Who's browsing")}: people who arrive from your emails and texts, sign up through your forms, or give their email at checkout or in a
          sign-up, login or contact form. Fields for someone else&apos;s address, like gift recipients, are skipped. This never subscribes anyone;
          their consent stays as it was.
        </li>
        <li>Activity from visitors it doesn&apos;t recognize yet waits in their browser for up to 3 days, and is sent once they&apos;re recognized.</li>
      </ul>
      <p className="mb-2 text-sm text-zinc-600">Custom site or app? Call these from your own code:</p>
      <CodeBlock code={customExample} label="JavaScript" />
      <p className="mb-2 mt-4 text-xs text-zinc-500">Calling it from code that may run before the snippet loads? Add this first:</p>
      <CodeBlock code={queueStub} label="HTML" />
      <p className="mb-2 mt-4 text-xs text-zinc-500">Don&apos;t want automatic tracking?</p>
      <CodeBlock code={optOut} label="HTML" />
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
      <CodeBlock code={`<div data-dumbmail-form="${formId}"></div>`} label="HTML" />
    </Card>
  );
}

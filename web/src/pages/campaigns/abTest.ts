import type { AbMetric, AbTest, Campaign, Channel, EmailContent, SmsContent, VersionId } from "../../api";
import { formatNumber, pluralize } from "../../lib/format";

type Content = EmailContent | SmsContent;
type VariantId = Exclude<VersionId, "a">;

export const VERSION_IDS: VersionId[] = ["a", "b", "c", "d"];
const VARIANT_IDS: VariantId[] = ["b", "c", "d"];
export const MAX_VERSIONS = VERSION_IDS.length;

export const TEST_DEFAULTS = { test_percent: 20, wait_hours: 4, metric: "click_rate", pick: "auto" } as const;

export const metricLabels: Record<AbMetric, string> = { open_rate: "Open rate", click_rate: "Click rate", revenue: "Revenue" };

export function versionLabel(v: VersionId) {
  return `Version ${v.toUpperCase()}`;
}

/** Mid-sentence form: "version B". */
export function versionName(v: VersionId) {
  return `version ${v.toUpperCase()}`;
}

/** The versions a test has: A (the campaign's own content) then B-D. No test means just A. */
export function versionIds(test: AbTest | null): VersionId[] {
  return VERSION_IDS.slice(0, 1 + (test?.variants.length ?? 0));
}

/** A test's settings with the server's defaults filled in. */
export function testSettings(test: AbTest) {
  return {
    test_percent: test.test_percent ?? TEST_DEFAULTS.test_percent,
    wait_hours: test.wait_hours ?? TEST_DEFAULTS.wait_hours,
    metric: test.metric ?? TEST_DEFAULTS.metric,
    pick: test.pick ?? TEST_DEFAULTS.pick,
  };
}

type Versioned = { content: Content; ab_test: AbTest | null };

export function versionContent({ content, ab_test }: Versioned, v: VersionId): Content {
  if (v === "a") return content;
  return ab_test?.variants.find((x) => x.id === v)?.content ?? content;
}

/** Replace one version's content: A is the campaign's content, B-D live in the test. */
export function withVersionContent<T extends Versioned>(current: T, v: VersionId, next: Content): T {
  if (v === "a" || !current.ab_test) return { ...current, content: next };
  return { ...current, ab_test: { ...current.ab_test, variants: current.ab_test.variants.map((x) => (x.id === v ? { ...x, content: next } : x)) } };
}

/** Start a test with version B as a copy of A. */
export function startTest<T extends Versioned>(current: T): T {
  return { ...current, ab_test: { ...TEST_DEFAULTS, variants: [{ id: "b", content: structuredClone(current.content) }] } };
}

/** Add the next version as a copy of `from`'s content. */
export function addVersion<T extends Versioned>(current: T, from: VersionId): T {
  const test = current.ab_test;
  if (!test) return startTest(current);
  const id = VARIANT_IDS[test.variants.length];
  if (!id) return current;
  return { ...current, ab_test: { ...test, variants: [...test.variants, { id, content: structuredClone(versionContent(current, from)) }] } };
}

/**
 * Drop a version and re-letter the rest so they stay b, c, d in order. Removing A promotes B to
 * the campaign's content. Down to one version, the test goes away.
 */
export function removeVersion<T extends Versioned>(current: T, v: VersionId): T {
  const test = current.ab_test;
  if (!test) return current;
  const contents = versionIds(test)
    .filter((id) => id !== v)
    .map((id) => versionContent(current, id));
  const [content, ...rest] = contents;
  if (!content) return current;
  return {
    ...current,
    content,
    ab_test: rest.length === 0 ? null : { ...test, variants: rest.map((c, i) => ({ id: VARIANT_IDS[i], content: c })) },
  };
}

/** How the server splits the audience: see `prepare_send` in src/campaigns.rs. */
export function testSplit(total: number, percent: number, versions: number) {
  const inTest = percent >= 100 ? total : Math.min(total, Math.max(versions, Math.ceil((total * percent) / 100)));
  return { inTest, perVersion: Math.floor(inTest / versions), rest: total - inTest };
}

/** One plain sentence on who gets what, e.g. "20% of 5,000 = 1,000 people get the test, 500 per version; the other 4,000 get the winner." */
export function splitSentence(test: AbTest, total: number | undefined) {
  const { test_percent: pct } = testSettings(test);
  const n = test.variants.length + 1;
  if (total === undefined) {
    return pct >= 100
      ? `Everyone gets one of the ${n} versions, split evenly. There's no winner send.`
      : `${pct}% of your audience gets the test, split evenly across ${n} versions; everyone else gets the winner.`;
  }
  const s = testSplit(total, pct, n);
  if (pct >= 100) return `Everyone (${formatNumber(total)}) gets one of the ${n} versions, about ${formatNumber(s.perVersion)} each. There's no winner send.`;
  return `${pct}% of ${formatNumber(total)} = ${pluralize(s.inTest, "person", "people")} get the test, about ${formatNumber(s.perVersion)} per version; the other ${formatNumber(s.rest)} get the winner.`;
}

export function waitLabel(hours: number) {
  if (hours % 168 === 0) return pluralize(hours / 168, "week");
  if (hours >= 24 && hours % 24 === 0) return pluralize(hours / 24, "day");
  return pluralize(hours, "hour");
}

/** What happens after the test slice goes out. */
export function afterTestSentence(test: AbTest) {
  const s = testSettings(test);
  if (s.test_percent >= 100) return null;
  const metric = metricLabels[s.metric].toLowerCase();
  return s.pick === "auto"
    ? `After ${waitLabel(s.wait_hours)}, the version with the best ${metric} goes to everyone else automatically.`
    : `Results are ready after ${waitLabel(s.wait_hours)}; then you pick which version goes to everyone else (you can pick sooner).`;
}

/** Metrics a channel can be judged on (SMS has no opens). */
export function metricsFor(channel: Channel): AbMetric[] {
  return channel === "email" ? ["open_rate", "click_rate", "revenue"] : ["click_rate", "revenue"];
}

/** A short label for a version's content: the subject line, or the start of the text. */
export function contentSnippet(content: Content, channel: Campaign["channel"]) {
  const text = channel === "email" ? (content as EmailContent).subject : (content as SmsContent).body;
  const trimmed = (text ?? "").trim();
  if (!trimmed) return channel === "email" ? "No subject" : "No message";
  return trimmed.length > 80 ? `${trimmed.slice(0, 79)}…` : trimmed;
}

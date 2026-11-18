/** A translatable text's state in one language. `default`: a link nobody localized. */
export type TranslationStatus = "missing" | "machine" | "check" | "reviewed" | "stale" | "default";

export type TranslationString = {
  id: string;
  /** Where it is, for people ("Subject", "Block 3 (button)"). */
  where: string;
  kind: "text" | "html" | "link";
  source: string;
  text: string;
  status: TranslationStatus;
};

export type StatusCounts = { missing: number; machine: number; check: number; reviewed: number; stale: number };

/** A message's translations in one language (from the stored message or sent content). */
export type TranslationView = {
  default_language: string;
  languages: string[];
  account_languages: string[];
  language: string | null;
  strings: TranslationString[];
  summary: Record<string, StatusCounts>;
};

/** The same, for content worked on in the editor, with the content after the change. */
export type ContentTranslation<C> = TranslationView & { content: C; import?: ImportSummary };

export type ImportSummary = { applied: number; empty: number; changed_since_export: string[]; refused: { id: string; why: string }[] };

export type LanguageSettings = {
  default_language: string;
  languages: string[];
  names: Record<string, string>;
  max_languages: number;
  machine_translation: { available: boolean; model: string; daily_characters: number; used_today: number; price_per_1000_characters_usd: number };
  system_text_languages: string[];
};

export type GlossaryTerm = { id: string; term: string; never_translate: boolean; translations: Record<string, string>; note: string; updated_at: string };

export type SystemText = { key: string; where: string; english: string; shipped: string | null; text: string | null; shown: string };
export type SystemTexts = { language: string; shipped: boolean; rtl: boolean; texts: SystemText[] };

export type TranslationRun = {
  id: string;
  target: { kind: string; id: string; part: string };
  language: string;
  status: "queued" | "running" | "done" | "failed";
  characters: number;
  translated: number;
  skipped: number;
  error: string | null;
  created_at: string;
  finished_at: string | null;
};

/** A stored message whose translations "Draft with AI" can fill in. */
export type TranslationTarget =
  | { kind: "campaign"; id: string; version?: string }
  | { kind: "flow"; id: string; step: string }
  | { kind: "template"; id: string }
  | { kind: "saved_block"; id: string };

export type LanguageResult = { language: string | null; name: string | null; sent: number; opened: number; clicked: number; unsubscribed: number; open_rate: number; click_rate: number };

/** Segment rule: the person's language. */
export type LanguageCondition = { type: "language"; op: "is" | "is_not" | "is_one_of"; value?: string; values?: string[] };

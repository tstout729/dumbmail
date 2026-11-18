import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import { toast } from "../../lib/toast";
import type { ContentTranslation, GlossaryTerm, LanguageResult, LanguageSettings, SystemTexts, TranslationRun, TranslationTarget, TranslationView } from "./types";

const keys = {
  settings: ["translations", "settings"] as const,
  glossary: ["translations", "glossary"] as const,
  texts: (lang: string) => ["translations", "texts", lang] as const,
  run: (id: string) => ["translations", "run", id] as const,
  report: (campaignId: string) => ["translations", "report", campaignId] as const,
};

/** Where a stored message's translations live in the API. */
export function targetPath(t: TranslationTarget): string {
  switch (t.kind) {
    case "campaign":
      return `/api/translations/campaigns/${t.id}`;
    case "flow":
      return `/api/translations/flows/${t.id}/steps/${encodeURIComponent(t.step)}`;
    case "template":
      return `/api/translations/templates/${t.id}`;
    case "saved_block":
      return `/api/translations/saved-blocks/${t.id}`;
  }
}

function version(t: TranslationTarget): string | undefined {
  return t.kind === "campaign" ? t.version : undefined;
}

export function useLanguageSettings() {
  return useQuery({ queryKey: keys.settings, queryFn: () => api<LanguageSettings>("GET", "/api/translations/settings"), staleTime: 60_000 });
}

export function useUpdateLanguageSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (b: { default_language?: string; languages?: string[] }) => api<LanguageSettings>("PUT", "/api/translations/settings", b),
    onSuccess: (s) => {
      qc.setQueryData(keys.settings, s);
      toast.success("Languages saved");
    },
  });
}

export function useGlossary() {
  return useQuery({ queryKey: keys.glossary, queryFn: () => api<{ data: GlossaryTerm[] }>("GET", "/api/translations/glossary") });
}

export function useSaveTerm() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...b }: { id?: string; term?: string; never_translate?: boolean; translations?: Record<string, string>; note?: string }) =>
      id ? api<{ id: string }>("PATCH", `/api/translations/glossary/${id}`, b) : api<{ id: string }>("POST", "/api/translations/glossary", b),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.glossary }),
  });
}

export function useDeleteTerm() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api<{ ok: boolean }>("DELETE", `/api/translations/glossary/${id}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.glossary }),
  });
}

export function useSystemTexts(lang: string) {
  return useQuery({ queryKey: keys.texts(lang), queryFn: () => api<SystemTexts>("GET", withQuery("/api/translations/system-texts", { language: lang })), enabled: lang !== "" });
}

export function useSaveSystemTexts() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (b: { language: string; texts: Record<string, string | null> }) => api<SystemTexts>("PUT", "/api/translations/system-texts", b),
    onSuccess: (v) => {
      qc.setQueryData(keys.texts(v.language), v);
      toast.success("Wording saved");
    },
  });
}

/** Content being edited, as the API reads it. */
export const contentApi = {
  inspect: <C>(content: C, language: string | null) => api<TranslationView>("POST", "/api/translations/inspect", { content, language }),
  apply: <C>(content: C, language: string, strings: { id: string; text: string | null; status?: "reviewed" | "machine" }[]) =>
    api<ContentTranslation<C>>("POST", "/api/translations/apply", { content, language, strings }),
  removeLanguage: <C>(content: C, language: string) => api<ContentTranslation<C>>("POST", "/api/translations/apply", { content, language, remove_language: true }),
  importFile: <C>(content: C, language: string, format: "xliff" | "csv", data: string) => api<ContentTranslation<C>>("POST", "/api/translations/import", { content, language, format, data }),
  preview: <C>(channel: "email" | "sms", content: C, language: string) =>
    api<{ subject?: string; html?: string; body?: string; language: string }>("POST", "/api/translations/preview", { channel, content, language }),
};

/** The stored message's translations (to pick up machine drafts). */
export function storedTranslations(t: TranslationTarget, language: string) {
  return api<TranslationView>("GET", withQuery(targetPath(t), { language, version: version(t) }));
}

/** What a run would cost, when it costs money (the run starts only after `confirm`). */
export type DraftQuote = { dry_run: true; strings: number; characters: number; price_usd: number };

export function startDraft(t: TranslationTarget, language: string, ids?: string[], confirm = false) {
  return api<TranslationRun | DraftQuote>("POST", `${targetPath(t)}/draft`, { language, ids, version: version(t), confirm });
}

/** A Draft with AI run; polls until it ends. */
export function useRun(id: string | null) {
  return useQuery({
    queryKey: keys.run(id ?? ""),
    queryFn: () => api<TranslationRun>("GET", `/api/translations/jobs/${id}`),
    enabled: id !== null,
    refetchInterval: (q) => (q.state.data && (q.state.data.status === "done" || q.state.data.status === "failed") ? false : 2_000),
  });
}

export function useLanguageReport(campaignId: string, enabled: boolean) {
  return useQuery({ queryKey: keys.report(campaignId), queryFn: () => api<{ data: LanguageResult[] }>("GET", withQuery("/api/translations/report", { campaign_id: campaignId })), enabled });
}

/** A language's name for people, from the browser ("French", "Portuguese (Brazil)"). */
export function languageName(tag: string): string {
  try {
    const n = new Intl.DisplayNames(["en"], { type: "language" }).of(tag);
    return n && n !== tag ? n : tag;
  } catch {
    return tag;
  }
}

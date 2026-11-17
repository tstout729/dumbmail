import type { ChangeEvent, ReactNode } from "react";
import { RotateCcw } from "lucide-react";
import type { FormConfig, FormKind, FormVariant, FormVariantField, VersionId } from "../../api";
import { Badge } from "../../components/ui";
import { VersionTabs } from "./FormAbTest";

/** What every editor section gets: the draft config and the ways to change it. */
export type Editor = {
  formId: string;
  /** The base config (version A) as drafted. */
  config: FormConfig;
  kind: FormKind;
  set: (patch: Partial<FormConfig>) => void;
  /** The account's brand color, used when the form has none. */
  accountBrandColor: string;
  timezone: string;
  testing: boolean;
  versions: VersionId[];
  version: VersionId;
  setVersion: (v: VersionId) => void;
  /** The selected variant (B-D), or undefined while version A is selected. */
  variant: FormVariant | undefined;
  setVariant: (patch: Partial<FormVariant>) => void;
  removeVersion: (id: FormVariant["id"]) => void;
};

export const isOverlayKind = (kind: FormKind) => kind === "popup" || kind === "flyout";

export type TextProps = {
  value: string;
  placeholder: string;
  onChange: (e: ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) => void;
};

/**
 * Props for a per-version text input. On version A it edits the form (empty = the default, shown as the placeholder);
 * on B-D it edits that version (empty = A's text, shown as the placeholder).
 */
export function versionText(ed: Editor, key: FormVariantField, fallback = ""): TextProps {
  if (ed.variant) {
    return {
      value: ed.variant[key] ?? "",
      placeholder: ed.config[key] || fallback,
      onChange: (e) => ed.setVariant({ [key]: e.target.value || undefined }),
    };
  }
  return {
    value: ed.config[key] ?? "",
    placeholder: fallback,
    onChange: (e) => ed.set({ [key]: e.target.value || undefined }),
  };
}

type SharedTextKey = "legal_text" | "success_button_text" | "success_button_url" | "redirect_url" | "confirm_message" | "sms_consent_text";

/** Props for a text input shared by every version. */
export function sharedText(ed: Editor, key: SharedTextKey, fallback = ""): TextProps {
  return { value: ed.config[key] ?? "", placeholder: fallback, onChange: (e) => ed.set({ [key]: e.target.value || undefined }) };
}

export const isHttpUrl = (v: string | null | undefined) => !v || /^https?:\/\/\S+$/i.test(v.trim());
export const urlError = (v: string | null | undefined) => (isHttpUrl(v) ? null : "Use a full link starting with https://");

/** Version tabs for a section header while a test runs. */
export function SectionVersionTabs({ ed }: { ed: Editor }) {
  return ed.testing ? <VersionTabs versions={ed.versions} value={ed.version} onChange={ed.setVersion} /> : null;
}

/** Marks a shared setting inside a per-version section while a test runs. */
export function AllVersions({ ed }: { ed: Editor }) {
  return ed.testing ? <Badge>All versions</Badge> : null;
}

/** A label-left, control-right row for compact settings like colors and toggles. */
export function Row({ label, hint, onReset, children }: { label: ReactNode; hint?: ReactNode; onReset?: () => void; children: ReactNode }) {
  return (
    <div className="flex min-h-8.5 items-center justify-between gap-3">
      <div className="min-w-0">
        <div className="flex items-center gap-1 text-sm text-zinc-700">
          {label}
          {onReset ? <ResetButton onClick={onReset} /> : null}
        </div>
        {hint ? <div className="text-xs text-zinc-500">{hint}</div> : null}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

export function ResetButton({ onClick, label = "Reset to default" }: { onClick: () => void; label?: string }) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      className="rounded p-0.5 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-700"
    >
      <RotateCcw className="size-3" />
    </button>
  );
}

/** Field label with a reset control when the value differs from the default. */
export function ResettableLabel({ children, onReset }: { children: ReactNode; onReset?: () => void }) {
  return (
    <span className="inline-flex items-center gap-1">
      {children}
      {onReset ? <ResetButton onClick={onReset} /> : null}
    </span>
  );
}

export const toInt = (v: string, min: number, max: number) => Math.min(max, Math.max(min, Math.round(Number(v) || 0)));

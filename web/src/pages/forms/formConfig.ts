import type {
  CustomFieldType,
  FlyoutPosition,
  FormAnimation,
  FormConfig,
  FormField,
  FormFieldDef,
  FormFont,
  FormKind,
  FormLayout,
  FormShadow,
  FormStyle,
  FormVariantField,
  PhoneStep,
  PopupPosition,
  StandardFieldKey,
  Targeting,
  Teaser,
  TeaserPosition,
} from "../../api";

// Defaults from docs/FORMS.md, shown as placeholders so an empty input reads as "uses the default".
export const contentDefaults = {
  headline: "Get 10% off your first order",
  body: "Join our list for early access to new arrivals and member-only offers.",
  button_text: "Sign up",
  success_message: "Thanks! Check your inbox.",
  confirm_message: "Almost done! Check your inbox and tap the link to confirm.",
  sms_consent_text:
    "By checking this box, you agree to receive recurring automated marketing text messages at this number. Consent is not a condition of purchase. Msg & data rates may apply. Reply STOP to opt out, HELP for help.",
} as const;

export const phoneStepDefaults = {
  headline: "Want texts too?",
  body: "Get early access by text.",
  button_text: "Text me",
  skip_text: "No thanks",
} as const;

export const defaultWidth: Record<FormKind, number> = { popup: 420, flyout: 360, embed: 480, page: 440 };

/** Style defaults that don't depend on the kind or brand color. */
export const styleDefaults = {
  layout: "stacked",
  align: "center",
  font: "system",
  headline_size: 24,
  headline_weight: 700,
  body_size: 15,
  background_color: "#ffffff",
  headline_color: "#111827",
  text_color: "#4b5563",
  input_background: "#ffffff",
  input_border_color: "#d1d5db",
  input_text_color: "#111827",
  corner_radius: 14,
  button_radius: 8,
  input_radius: 8,
  padding: 28,
  border_color: "#e5e7eb",
  overlay_color: "#111827",
  overlay_opacity: 55,
  close_color: "#9ca3af",
  animation: "fade",
} as const satisfies FormStyle;

export const kindStyleDefaults = (kind: FormKind) => ({
  width: defaultWidth[kind],
  border_width: kind === "embed" || kind === "page" ? 1 : 0,
  shadow: (kind === "popup" || kind === "flyout" ? "strong" : "none") as FormShadow,
  position: (kind === "flyout" ? "bottom_right" : "center") as PopupPosition | FlyoutPosition,
});

export const layouts: { value: FormLayout; label: string }[] = [
  { value: "stacked", label: "Image on top" },
  { value: "image_left", label: "Image left" },
  { value: "image_right", label: "Image right" },
  { value: "background", label: "Image behind" },
];

export const fonts: { value: FormFont; label: string }[] = [
  { value: "system", label: "System (clean sans-serif)" },
  { value: "inherit", label: "Same as my website" },
  { value: "serif", label: "System serif" },
  { value: "Inter", label: "Inter" },
  { value: "Poppins", label: "Poppins" },
  { value: "Montserrat", label: "Montserrat" },
  { value: "Lato", label: "Lato" },
  { value: "DM Sans", label: "DM Sans" },
  { value: "Space Grotesk", label: "Space Grotesk" },
  { value: "Playfair Display", label: "Playfair Display" },
  { value: "Lora", label: "Lora" },
  { value: "Merriweather", label: "Merriweather" },
  { value: "Oswald", label: "Oswald" },
];

export const animations: { value: FormAnimation; label: string }[] = [
  { value: "fade", label: "Fade" },
  { value: "slide", label: "Slide" },
  { value: "zoom", label: "Zoom" },
  { value: "none", label: "None" },
];

export const popupPositions: { value: PopupPosition; label: string }[] = [
  { value: "top", label: "Top" },
  { value: "center", label: "Center" },
  { value: "bottom", label: "Bottom" },
];

export const flyoutPositions: { value: FlyoutPosition; label: string }[] = [
  { value: "top_left", label: "Top left" },
  { value: "top_right", label: "Top right" },
  { value: "bottom_left", label: "Bottom left" },
  { value: "bottom_right", label: "Bottom right" },
];

export const teaserPositions: { value: TeaserPosition; label: string }[] = [
  { value: "bottom_left", label: "Bottom left" },
  { value: "bottom_right", label: "Bottom right" },
  { value: "left", label: "Left side" },
  { value: "right", label: "Right side" },
];

export const cssHooks = [
  ".dm-card",
  ".dm-media",
  ".dm-content",
  ".dm-headline",
  ".dm-body",
  ".dm-field",
  ".dm-label",
  ".dm-input",
  ".dm-consent",
  ".dm-button",
  ".dm-no-thanks",
  ".dm-legal",
  ".dm-error",
  ".dm-success",
  ".dm-code",
  ".dm-close",
  ".dm-overlay",
  ".dm-teaser",
];

// ---- Fields ----

export const standardFields: Record<StandardFieldKey, { label: string; placeholder: string }> = {
  email: { label: "Email", placeholder: "Email address" },
  phone_number: { label: "Phone number", placeholder: "Mobile number" },
  first_name: { label: "First name", placeholder: "First name" },
  last_name: { label: "Last name", placeholder: "Last name" },
  birthday: { label: "Birthday", placeholder: "Birthday" },
};

export const isStandardKey = (key: string): key is StandardFieldKey => key in standardFields;

export const customFieldTypes: { value: CustomFieldType; label: string }[] = [
  { value: "text", label: "Short text" },
  { value: "number", label: "Number" },
  { value: "date", label: "Date" },
  { value: "select", label: "Dropdown" },
  { value: "radio", label: "Pick one (buttons)" },
  { value: "checkboxes", label: "Pick any (checkboxes)" },
  { value: "checkbox", label: "Single checkbox (yes/no)" },
];

export const needsOptions = (type: CustomFieldType | undefined) => type === "select" || type === "radio" || type === "checkboxes";
export const maxFields = 12;
export const customKeyPattern = /^[a-z][a-z0-9_]{0,39}$/;
/** Profile columns and sign-up request keys a custom field can't use (the server rejects them). */
const reservedKeys = new Set([
  "id", "external_id", "properties", "email_consent", "sms_consent", "created_at", "updated_at", "last_event_at", "list_ids",
  "form_id", "variant", "step", "fields",
]);

/** The form's fields: `fields` when set, otherwise derived from the legacy `collect` list. */
export function fieldsOf(config: FormConfig): FormFieldDef[] {
  if (config.fields?.length) return config.fields;
  const collect = config.collect?.length ? config.collect : ["email" as const];
  // The old renderer drew first name, then email, then phone, whatever order `collect` lists them in.
  const order: FormField[] = ["first_name", "email", "phone_number"];
  return order.filter((k) => collect.includes(k)).map((key) => ({ key }));
}

export const hasPhoneField = (config: FormConfig) => fieldsOf(config).some((f) => f.key === "phone_number");

/** Field problems the server would reject, so Save can say what's wrong up front. */
export function fieldProblems(fields: FormFieldDef[]): string[] {
  const out: string[] = [];
  if (!fields.some((f) => f.key === "email" || f.key === "phone_number")) out.push("Add an email or phone number field so sign-ups can be saved.");
  const seen = new Set<string>();
  for (const f of fields) {
    const name = f.label || f.key || "A custom field";
    if (!isStandardKey(f.key) && !customKeyPattern.test(f.key)) out.push(`${name}: the saved-as name must start with a letter and use only a-z, 0-9 and _.`);
    else if (reservedKeys.has(f.key)) out.push(`${name}: "${f.key}" is used by DumbMail itself. Pick another saved-as name.`);
    if (seen.has(f.key)) out.push(`Two fields save to "${f.key}". Give each a different name.`);
    seen.add(f.key);
    if (needsOptions(f.type) && !(f.options ?? []).some((o) => o.trim())) out.push(`${name}: add at least one option.`);
  }
  return out;
}

/** A custom-property key from a label: "Skin type" → "skin_type". */
export function keyFromLabel(label: string): string {
  const k = label
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^[^a-z]+/, "")
    .replace(/_+$/, "")
    .slice(0, 40);
  return k || "field";
}

// ---- A/B versions ----

/** Keys a version can change. Everything else in the config is shared by all versions. */
export const variantFields: FormVariantField[] = [
  "headline",
  "body",
  "button_text",
  "image_url",
  "image_alt",
  "no_thanks_text",
  "success_message",
  "success_body",
  "discount_code",
];

// ---- Saving ----

const topLevelKeys = [
  ...variantFields,
  "coupon",
  "legal_text",
  "success_button_text",
  "success_button_url",
  "redirect_url",
  "auto_close_seconds",
  "confirm_message",
  "sms_consent_text",
  "brand_color",
  "fields",
  "collect",
  "phone_step",
  "style",
  "teaser",
  "delay_seconds",
  "targeting",
  "variants",
] as const satisfies readonly (keyof FormConfig)[];

const styleKeys = [
  "layout", "width", "align", "font", "headline_size", "headline_weight", "body_size", "background_color", "headline_color",
  "text_color", "button_color", "button_text_color", "button_full_width", "input_background", "input_border_color",
  "input_text_color", "corner_radius", "button_radius", "input_radius", "padding", "border_width", "border_color", "shadow",
  "overlay_color", "overlay_opacity", "close_on_overlay", "close_color", "position", "animation", "page_background", "custom_css",
] as const satisfies readonly (keyof FormStyle)[];
const targetingKeys = [
  "trigger", "scroll_percent", "devices", "url_contains", "url_excludes", "page_views", "visitors", "frequency_days",
  "hide_for_subscribers", "start_at", "end_at",
] as const satisfies readonly (keyof Targeting)[];
const teaserKeys = ["enabled", "text", "position", "show"] as const satisfies readonly (keyof Teaser)[];
const phoneStepKeys = ["enabled", "headline", "body", "button_text", "skip_text"] as const satisfies readonly (keyof PhoneStep)[];
const fieldKeys = ["key", "type", "label", "placeholder", "required", "options"] as const satisfies readonly (keyof FormFieldDef)[];

const blank = (v: unknown) => v === undefined || v === null || v === "" || (Array.isArray(v) && v.length === 0);

/** Only the listed keys with a value: the server rejects unknown keys, and an empty value means "use the default". */
function pick<T extends object>(obj: T, allowed: readonly (keyof T)[]): Partial<T> {
  const out: Partial<T> = {};
  for (const k of allowed) if (!blank(obj[k])) out[k] = obj[k];
  return out;
}

const pickOrNull = <T extends object>(obj: T | null | undefined, allowed: readonly (keyof T)[]) => {
  if (!obj) return null;
  const out = pick(obj, allowed);
  return Object.keys(out).length ? out : null;
};

/**
 * The config to send. PATCH replaces each top-level key it gets and keeps the rest, so a key the saved form has but the
 * draft cleared goes out as `null` (back to the default). Nested objects and lists always go whole.
 */
export function configForSave(draft: FormConfig, saved: FormConfig): FormConfig {
  const out: Record<string, unknown> = {};
  for (const k of topLevelKeys) {
    let v: unknown = draft[k];
    if (k === "style") v = pickOrNull(draft.style, styleKeys);
    else if (k === "targeting") v = pickOrNull(draft.targeting, targetingKeys);
    else if (k === "teaser") v = pickOrNull(draft.teaser, teaserKeys);
    else if (k === "phone_step") v = pickOrNull(draft.phone_step, phoneStepKeys);
    else if (k === "fields") v = draft.fields ? draft.fields.map((f) => pick(f, fieldKeys)) : null;
    else if (k === "variants") v = draft.variants ? draft.variants.map((x) => ({ ...pick(x, variantFields), id: x.id })) : null;
    else if (typeof v === "string" && v.trim() === "") v = null;
    if (k === "variants" && Array.isArray(v)) out[k] = v; // [] ends a test; keep it.
    else if (!blank(v)) out[k] = v;
    else if (!blank(saved[k])) out[k] = null;
  }
  return out as FormConfig;
}


// ---- Colors (same math as the renderer, to show the defaults it will pick) ----

const rgbOf = (hex: string) => {
  let h = hex.slice(1);
  if (h.length <= 4) h = h.replace(/./g, (c) => c + c);
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16));
};
export const isHex = (v: string | null | undefined): v is string => !!v && /^#(?:[0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(v.trim());

/** White or black, whichever reads better on `hex`: the default button text color. */
export function readableOn(hex: string): string {
  const [r, g, b] = rgbOf(hex).map((v) => {
    const x = v / 255;
    return x <= 0.03928 ? x / 12.92 : Math.pow((x + 0.055) / 1.055, 2.4);
  });
  const l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return 1.05 / (l + 0.05) >= (l + 0.05) / 0.05 ? "#ffffff" : "#000000";
}

/** `hex` mixed with white (0 = the color, 1 = white): the default hosted-page background is tint(button, 0.9). */
export function tint(hex: string, amount: number): string {
  return `#${rgbOf(hex)
    .map((v) => Math.round(v + (255 - v) * amount).toString(16).padStart(2, "0"))
    .join("")}`;
}

import { FormCouponField } from "../../features/coupons/FormCouponField";
import { PartyPopper, Type } from "lucide-react";
import { CollapsibleCard, Field, Input, SubHeading, Textarea } from "../../components/ui";
import { contentDefaults } from "./formConfig";
import { ImageInput } from "./ImageInput";
import { versionLabel } from "./FormAbTest";
import { AllVersions, SectionVersionTabs, isOverlayKind, sharedText, toInt, urlError, versionText, type Editor } from "./editorShared";

type SectionProps = { ed: Editor; open: boolean; onToggle: () => void };

function VersionStrip({ ed }: { ed: Editor }) {
  if (!ed.variant) return null;
  const id = ed.variant.id;
  return (
    <div className="flex items-start justify-between gap-2 rounded-md bg-violet-50 px-3 py-2 text-xs text-violet-900">
      <span>
        Editing {versionLabel(id)}. Empty boxes use version A&rsquo;s text.
      </span>
      <button type="button" className="shrink-0 font-medium text-red-700 hover:underline" onClick={() => ed.removeVersion(id)}>
        Remove version {id.toUpperCase()}
      </button>
    </div>
  );
}

export function ContentSection({ ed, open, onToggle }: SectionProps) {
  const c = ed.config;
  const image = versionText(ed, "image_url");
  const title = ed.testing ? `Content · ${versionLabel(ed.version)}` : "Content";
  return (
    <CollapsibleCard
      title={title}
      icon={<Type />}
      summary={c.headline || contentDefaults.headline}
      open={open}
      onToggle={onToggle}
      actions={<SectionVersionTabs ed={ed} />}
    >
      <VersionStrip ed={ed} />
      <Field label="Headline">
        <Input maxLength={200} {...versionText(ed, "headline", contentDefaults.headline)} />
      </Field>
      <Field label="Text" hint="Line breaks are kept.">
        <Textarea rows={3} maxLength={1000} {...versionText(ed, "body", contentDefaults.body)} />
      </Field>
      <Field label="Button">
        <Input maxLength={60} {...versionText(ed, "button_text", contentDefaults.button_text)} />
      </Field>
      {isOverlayKind(ed.kind) ? (
        <Field label="“No thanks” link" hint="A link under the button that closes the form. Leave empty for none.">
          <Input maxLength={60} {...versionText(ed, "no_thanks_text", "e.g. No thanks, I'll pay full price")} />
        </Field>
      ) : null}
      <Field label="Image" hint="Where it sits is set under Style → Layout.">
        <ImageInput
          value={image.value}
          placeholder={image.placeholder}
          onChange={(url) => (ed.variant ? ed.setVariant({ image_url: url || undefined }) : ed.set({ image_url: url || undefined }))}
        />
      </Field>
      <Field label="Image description" hint="For screen readers. Describe what the image shows.">
        <Input maxLength={200} {...versionText(ed, "image_alt", "e.g. Model wearing the linen shirt")} />
      </Field>
      <SubHeading action={<AllVersions ed={ed} />}>Small print</SubHeading>
      <Field label="Small print under the button" hint="Write [label](https://link) to add a link, e.g. [Privacy policy](https://…/privacy).">
        <Textarea rows={2} maxLength={1000} {...sharedText(ed, "legal_text", "e.g. You can unsubscribe at any time. [Privacy policy](https://…)")} />
      </Field>
    </CollapsibleCard>
  );
}

export function SuccessSection({ ed, open, onToggle }: SectionProps) {
  const c = ed.config;
  const overlay = isOverlayKind(ed.kind);
  const autoDefault = c.discount_code || c.success_button_text ? 0 : 3;
  const title = ed.testing ? `Thank-you screen · ${versionLabel(ed.version)}` : "Thank-you screen";
  return (
    <CollapsibleCard
      title={title}
      icon={<PartyPopper />}
      summary={c.redirect_url ? `Sends people to ${c.redirect_url}` : c.success_message || contentDefaults.success_message}
      open={open}
      onToggle={onToggle}
      actions={<SectionVersionTabs ed={ed} />}
    >
      <VersionStrip ed={ed} />
      <Field label="Message">
        <Input maxLength={200} {...versionText(ed, "success_message", contentDefaults.success_message)} />
      </Field>
      <Field label="Text under it">
        <Textarea rows={2} maxLength={1000} {...versionText(ed, "success_body", "e.g. Your code works on everything, including sale items.")} />
      </Field>
      <Field label="Discount code" hint="Shown in a box with a Copy button.">
        <Input maxLength={60} className="font-mono" {...versionText(ed, "discount_code", "e.g. WELCOME10")} />
      </Field>
      <FormCouponField value={c.coupon} onChange={(coupon) => ed.set({ coupon })} />
      <SubHeading action={<AllVersions ed={ed} />}>Button and next step</SubHeading>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Button text" hint="Leave empty for no button.">
          <Input maxLength={60} {...sharedText(ed, "success_button_text", "e.g. Start shopping")} />
        </Field>
        <Field label="Button link" error={urlError(c.success_button_url)} hint={overlay ? "Empty closes the form." : undefined}>
          <Input type="url" {...sharedText(ed, "success_button_url", "https://…")} />
        </Field>
      </div>
      <Field
        label="Send people to a page instead"
        error={urlError(c.redirect_url)}
        hint="Right after they sign up, skipping the thank-you screen. Leave empty to show the screen."
      >
        <Input type="url" {...sharedText(ed, "redirect_url", "https://…/welcome")} />
      </Field>
      {overlay ? (
        <Field label="Close the thank-you screen after" hint={`0 keeps it open. Default: ${autoDefault} seconds.`}>
          <div className="flex items-center gap-2">
            <Input
              type="number"
              min={0}
              max={60}
              className="w-24"
              value={c.auto_close_seconds ?? ""}
              placeholder={String(autoDefault)}
              onChange={(e) => ed.set({ auto_close_seconds: e.target.value === "" ? undefined : toInt(e.target.value, 0, 60) })}
            />
            <span className="text-sm text-zinc-600">seconds</span>
          </div>
        </Field>
      ) : null}
      <Field label="Double opt-in message" hint="Shown instead of the thank-you screen when the list asks people to confirm by email.">
        <Textarea rows={2} maxLength={300} {...sharedText(ed, "confirm_message", contentDefaults.confirm_message)} />
      </Field>
    </CollapsibleCard>
  );
}

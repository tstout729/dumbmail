import { useState } from "react";
import { ImageOff, X } from "lucide-react";
import { socialLabels, socialNetworks, type SocialNetwork } from "../../api";
import { cx } from "../ui";

export type SocialLinks = Partial<Record<SocialNetwork, string>>;

const freeMail = new Set([
  "gmail.com",
  "googlemail.com",
  "yahoo.com",
  "ymail.com",
  "outlook.com",
  "hotmail.com",
  "live.com",
  "msn.com",
  "icloud.com",
  "me.com",
  "mac.com",
  "aol.com",
  "proton.me",
  "protonmail.com",
  "gmx.com",
  "mail.com",
]);

/** The domain of a work email ("ann@acme.com" → "acme.com"), or null for free mail like Gmail. */
export function workDomain(email: string | null | undefined): string | null {
  const domain = email?.split("@")[1]?.toLowerCase();
  return domain && !freeMail.has(domain) ? domain : null;
}

export function isFreeMail(email: string | null | undefined): boolean {
  const domain = email?.split("@")[1]?.toLowerCase();
  return !domain || freeMail.has(domain);
}

/** A small framed preview of a logo URL, with a placeholder when it's missing or won't load. */
export function LogoThumb({ url, className }: { url: string; className?: string }) {
  const [failed, setFailed] = useState<string | null>(null);
  const broken = !url || failed === url;
  return (
    <div className={cx("flex h-14 w-28 shrink-0 items-center justify-center overflow-hidden rounded-lg border border-zinc-200 bg-white p-2", className)}>
      {broken ? (
        <ImageOff className="size-5 text-zinc-300" aria-label={url ? "Logo didn't load" : "No logo"} />
      ) : (
        <img src={url} alt="Logo preview" className="max-h-full max-w-full object-contain" onError={() => setFailed(url)} />
      )}
    </div>
  );
}

/** Clickable color dots for the colors found on a site. */
export function ColorSwatches({ colors, value, onPick }: { colors: string[]; value: string; onPick: (color: string) => void }) {
  if (colors.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-2" role="group" aria-label="Colors from your site">
      {colors.map((c) => {
        const selected = value.toLowerCase() === c.toLowerCase();
        return (
          <button
            key={c}
            type="button"
            title={c}
            aria-label={`Use ${c}`}
            aria-pressed={selected}
            onClick={() => onPick(c)}
            className={cx("size-8 rounded-full ring-offset-2 transition-shadow", selected ? "ring-2 ring-zinc-900" : "ring-1 ring-zinc-200 hover:ring-zinc-400")}
            style={{ background: c }}
          />
        );
      })}
    </div>
  );
}

/** Social links as removable chips. */
export function SocialChips({ links, onRemove }: { links: SocialLinks; onRemove?: (network: SocialNetwork) => void }) {
  const present = socialNetworks.filter((n) => links[n]);
  if (present.length === 0) return null;
  return (
    <ul className="flex flex-wrap gap-1.5">
      {present.map((n) => (
        <li key={n} className="inline-flex max-w-full items-center gap-1 rounded-full border border-zinc-200 bg-zinc-50 py-0.5 pr-1 pl-2.5 text-xs text-zinc-700">
          <a href={links[n]} target="_blank" rel="noreferrer" className="truncate hover:underline" title={links[n]}>
            {socialLabels[n]}
          </a>
          {onRemove ? (
            <button type="button" onClick={() => onRemove(n)} aria-label={`Remove ${socialLabels[n]}`} className="rounded-full p-0.5 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-700">
              <X className="size-3" />
            </button>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

type PreviewProps = { name: string; logoUrl: string; color: string; address: string; socials: SocialLinks };

/** A miniature of how emails will look with this brand. */
export function BrandEmailPreview({ name, logoUrl, color, address, socials }: PreviewProps) {
  const [failed, setFailed] = useState<string | null>(null);
  const showLogo = logoUrl && failed !== logoUrl;
  const present = socialNetworks.filter((n) => socials[n]);
  return (
    <div className="rounded-xl bg-zinc-100 p-3 sm:p-4" aria-label="Email preview">
      <div className="mx-auto max-w-sm overflow-hidden rounded-lg bg-white shadow-sm">
        <div className="flex h-14 items-center justify-center px-4 pt-3">
          {showLogo ? (
            <img src={logoUrl} alt="" className="max-h-9 max-w-[60%] object-contain" onError={() => setFailed(logoUrl)} />
          ) : (
            <span className="truncate text-sm font-semibold text-zinc-900">{name || "Your brand"}</span>
          )}
        </div>
        <div className="space-y-2 px-5 pt-3 pb-5">
          <div className="h-3 w-2/3 rounded bg-zinc-200" />
          <div className="h-2 w-full rounded bg-zinc-100" />
          <div className="h-2 w-5/6 rounded bg-zinc-100" />
          <div className="pt-2">
            <span className="inline-block rounded-md px-4 py-1.5 text-xs font-semibold text-white" style={{ background: color }}>
              Shop now
            </span>
          </div>
        </div>
      </div>
      <p className="mx-auto mt-2 max-w-sm text-center text-[10px] leading-relaxed text-zinc-500">
        {present.length ? <span className="block">{present.map((n) => socialLabels[n]).join(" · ")}</span> : null}
        {name || "Your brand"}
        {address ? ` · ${address.replace(/\s*\n\s*/g, ", ")}` : " · (mailing address)"}
        <span className="block underline">Unsubscribe</span>
      </p>
    </div>
  );
}

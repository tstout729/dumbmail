import { Bookmark, LayoutPanelLeft, Menu, PanelBottom, PanelTop, Quote, Share2, Table, Timer, Video, type LucideIcon } from "lucide-react";
import type { EmailBlock } from "../../api/types";
import type { ContentBlock } from "./types";

export type ContentBlockType = ContentBlock["type"];

export const contentBlockMeta: Record<ContentBlockType, { label: string; icon: LucideIcon }> = {
  image_text: { label: "Image beside text", icon: LayoutPanelLeft },
  social: { label: "Social links", icon: Share2 },
  video: { label: "Video", icon: Video },
  countdown: { label: "Countdown timer", icon: Timer },
  table: { label: "Table", icon: Table },
  quote: { label: "Quote", icon: Quote },
  header: { label: "Header", icon: PanelTop },
  nav: { label: "Menu bar", icon: Menu },
  footer: { label: "Footer", icon: PanelBottom },
  saved: { label: "Saved block", icon: Bookmark },
};

/** New blocks offered in the add menu (saved blocks are inserted from their own picker). */
export const contentBlockOrder: ContentBlockType[] = ["image_text", "video", "social", "countdown", "table", "quote", "header", "nav", "footer"];

/** A countdown's default end: a week from now, on the hour. */
function nextWeek(): string {
  const d = new Date(Date.now() + 7 * 86_400_000);
  d.setMinutes(0, 0, 0);
  return d.toISOString();
}

export function newContentBlock(type: ContentBlockType): ContentBlock {
  switch (type) {
    case "social":
      return { type, style: "color", size: 32, align: "center" };
    case "video":
      return { type, url: "https://", alt: "Watch the video" };
    case "countdown":
      return { type, ends_at: nextWeek(), background: "#111827", color: "#ffffff", label_color: "#9ca3af", ended_text: "This offer has ended" };
    case "table":
      return { type, columns: [{ label: "Item" }, { label: "Price", align: "right" }], rows: [["Your item", "$0.00"]] };
    case "quote":
      return { type, text: "A short, specific thing a customer said.", author: "Customer name", style: "plain" };
    case "header":
      return { type, links: [{ text: "Shop", href: "https://" }, { text: "About", href: "https://" }] };
    case "nav":
      return { type, links: [{ text: "Shop", href: "https://" }, { text: "New", href: "https://" }, { text: "Sale", href: "https://" }] };
    case "footer":
      return { type, text: "Questions? Just reply to this email.", show_social: true, style: "light" };
    case "image_text":
      return { type, src: "", alt: "", heading: "A headline", html: "<p>A sentence or two about it.</p>", button_text: "Learn more", button_href: "https://", image_position: "left" };
    case "saved":
      return { type, saved_block_id: "", blocks: [] };
  }
}

function strip(html: string) {
  return html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim();
}

export function contentBlockSummary(b: ContentBlock): string {
  switch (b.type) {
    case "social":
      return b.links?.length ? b.links.map((l) => l.network).join(", ") : "Your account's social links";
    case "video":
      return b.thumbnail ? b.url : `${b.url} · no thumbnail yet`;
    case "countdown":
      return `Ends ${new Date(b.ends_at).toLocaleString()}`;
    case "table":
      return `${b.columns.length} columns${b.loop ? " · a row per event item" : ""}`;
    case "quote":
      return [b.text, b.author].filter(Boolean).join(" — ");
    case "header":
    case "nav":
      return (b.links ?? []).map((l) => l.text).join(" · ") || "No links yet";
    case "footer":
      return b.text || "Footer";
    case "image_text":
      return b.heading || strip(b.html ?? "") || "Image beside text";
    case "saved":
      return b.name ?? "Saved block";
  }
}

export function isContentType(t: string): t is ContentBlockType {
  return t in contentBlockMeta;
}

export function isContentBlock(b: EmailBlock): b is EmailBlock & ContentBlock {
  return isContentType(b.type);
}

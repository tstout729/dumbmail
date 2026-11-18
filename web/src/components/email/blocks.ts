import {
  AlignLeft,
  Code,
  Columns2,
  Heading,
  Image,
  Minus,
  MousePointerClick,
  LayoutGrid,
  MoveVertical,
  ShoppingBag,
  Star,
  StarHalf,
  TicketPercent,
  type LucideIcon,
} from "lucide-react";
import type { EmailBlock, EmailBlockType, EmailContent } from "../../api";
import { newProductsBlock, sourceLabel } from "../../features/catalog/ProductsBlockFields";
import { contentBlockMeta, contentBlockOrder, contentBlockSummary, isContentBlock, isContentType, newContentBlock } from "../../features/content/blockDefs";
import { couponBlockSummary, newCouponBlock } from "../../features/coupons/CouponBlockFields";
import { newReviewRequestBlock, newReviewsBlock, reviewRequestBlockSummary, reviewsBlockSummary } from "../../features/reviews/BlockFields";

export const blockMeta: Record<EmailBlockType, { label: string; icon: LucideIcon }> = {
  heading: { label: "Heading", icon: Heading },
  text: { label: "Text", icon: AlignLeft },
  image: { label: "Image", icon: Image },
  button: { label: "Button", icon: MousePointerClick },
  divider: { label: "Divider", icon: Minus },
  spacer: { label: "Spacer", icon: MoveVertical },
  columns: { label: "Columns", icon: Columns2 },
  product: { label: "Product", icon: ShoppingBag },
  products: { label: "Products from catalog", icon: LayoutGrid },
  coupon: { label: "Coupon code", icon: TicketPercent },
  review_request: { label: "Review request", icon: Star },
  reviews: { label: "Reviews", icon: StarHalf },
  html: { label: "HTML", icon: Code },
  ...contentBlockMeta,
};

export const blockOrder: EmailBlockType[] = ["heading", "text", "image", "button", "products", "product", "coupon", "review_request", "reviews", "columns", "divider", "spacer", "html", ...contentBlockOrder];

export function newBlock(type: EmailBlockType): EmailBlock {
  if (isContentType(type)) return newContentBlock(type);
  switch (type) {
    case "heading":
      return { type, text: "A headline that earns the open", level: 1, align: "left" };
    case "text":
      return { type, html: "<p>Hi {{ first_name|default(\"there\") }},</p><p>Write something worth reading.</p>", align: "left" };
    case "image":
      return { type, src: "", alt: "" };
    case "button":
      return { type, text: "Shop now", href: "https://", align: "center" };
    case "divider":
      return { type };
    case "spacer":
      return { type, height: 24 };
    case "columns":
      return {
        type,
        columns: [
          [{ type: "text", html: "<p>Left column</p>" }],
          [{ type: "text", html: "<p>Right column</p>" }],
        ],
      };
    case "product":
      return { type, title: "Product name", price: "$29", href: "https://", button_text: "Buy now" };
    case "html":
      return { type, html: "<p>Custom HTML</p>" };
    case "products":
      return newProductsBlock();
    case "coupon":
      return newCouponBlock();
    case "reviews":
      return newReviewsBlock();
    case "review_request":
      return newReviewRequestBlock();
  }
}

function stripTags(html: string) {
  return html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim();
}

export function blockSummary(b: EmailBlock): string {
  if (isContentBlock(b)) return contentBlockSummary(b);
  switch (b.type) {
    case "heading":
      return b.text || "Empty heading";
    case "text":
      return stripTags(b.html) || "Empty text";
    case "image":
      return b.src ? b.alt || b.src : "No image yet";
    case "button":
      return `${b.text} → ${b.href}`;
    case "divider":
      return "Horizontal line";
    case "spacer":
      return `${b.height ?? 24}px of space`;
    case "columns":
      return `${b.columns.length} columns`;
    case "product":
      return [b.title, b.price].filter(Boolean).join(" · ");
    case "html":
      return stripTags(b.html) || "Custom HTML";
    case "products":
      return `${b.count ?? 4} × ${sourceLabel(b.source)}`;
    case "coupon":
      return couponBlockSummary(b);
    case "reviews":
      return reviewsBlockSummary(b);
    case "review_request":
      return reviewRequestBlockSummary(b);
  }
}

export function starterEmail(brandColor?: string): EmailContent {
  return {
    subject: "",
    preheader: "",
    blocks: [newBlock("heading"), newBlock("text"), newBlock("button")],
    theme: brandColor ? { brand_color: brandColor } : undefined,
  };
}

export const templateVariables: { label: string; value: string }[] = [
  { label: "First name", value: '{{ first_name|default("there") }}' },
  { label: "Last name", value: "{{ person.last_name }}" },
  { label: "Email", value: "{{ person.email }}" },
  { label: "Custom property", value: "{{ person.properties.city }}" },
  { label: "Company name", value: "{{ organization.name }}" },
  { label: "Event item (flows)", value: "{{ event.properties.items[0].name }}" },
  { label: "A product by its ID", value: '{{ catalog.products["YOUR-ID"].title }}' },
  { label: "Top pick for this person", value: "{{ recommendations[0].title }} {{ recommendations[0].url }}" },
  { label: "Unique coupon code", value: "{% coupon_code 'COUPON NAME' %}" },
  { label: "Review link", value: "{% review_link %}" },
  { label: "Unsubscribe link", value: "{{ unsubscribe_url }}" },
];

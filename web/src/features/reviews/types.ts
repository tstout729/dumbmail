import type { EmailContent, SmsContent } from "../../api/types";

export type ReviewStatus = "pending" | "published" | "rejected" | "removed";
export type ReviewSource = "request" | "public" | "import";
export type RejectReason = "spam" | "offensive" | "personal_info" | "not_about_this" | "fake" | "other";
export type SubjectKind = "product" | "listing" | "service";

export type ReviewPhoto = { id: string; url: string; width: number; height: number };
export type ReviewAnswer = { id: string; label: string; answer: string };

export type Review = {
  id: string;
  status: ReviewStatus;
  reject_reason: RejectReason | null;
  rating: number;
  title: string;
  body: string;
  display_name: string;
  recommend: boolean | null;
  answers: ReviewAnswer[];
  verified: boolean;
  /** Imports: the source marked it verified (never shown publicly as a badge). */
  verified_by_source: boolean;
  source: ReviewSource;
  imported: boolean;
  incentivized: boolean;
  flags: string[];
  featured: boolean;
  reply: string | null;
  replied_at: string | null;
  awaiting_confirmation: boolean;
  reviewed_at: string;
  created_at: string;
  edited_at: string | null;
  published_at: string | null;
  product: { id: string; product_id: string; name: string; image_url: string | null; url: string | null; kind: SubjectKind };
  photos: ReviewPhoto[];
  /** Only for people who may see profiles. */
  author?: { profile_id: string; email: string | null; first_name: string | null; last_name: string | null };
};

export type ReviewCounts = Partial<Record<ReviewStatus | "awaiting_confirmation", number>>;
export type ReviewList = { data: Review[]; total: number; counts: ReviewCounts; note: string };

export type ReviewQuery = {
  status?: ReviewStatus;
  rating?: number;
  product_id?: string;
  verified?: boolean;
  has_photos?: boolean;
  source?: ReviewSource;
  unconfirmed?: boolean;
  since?: string;
  until?: string;
  q?: string;
  limit?: number;
  offset?: number;
};

export type ModerateAction = "publish" | "reject" | "hold" | "feature" | "unfeature";
export type ModerateInput = { ids: string[]; action: ModerateAction; reason?: RejectReason; confirm?: boolean };
export type ModerateResult = { ok?: boolean; dry_run?: boolean; changed?: string[]; unchanged?: number; low_ratings?: number; note?: string | null };

export type Question = { id: string; label: string; kind: "choice" | "text"; options?: string[] };
export type WidgetSettings = { heading?: string; star_color?: string; text_color?: string; accent_color?: string; per_page?: number };

export type ReviewSettings = {
  auto_publish: boolean;
  auto_publish_min_rating: number;
  flag_words: string[];
  questions: Question[];
  public_reviews: boolean;
  request_days: number;
  thank_you_coupon: string | null;
  subscribe_form_id: string | null;
  widget: WidgetSettings;
  purchase_metrics: string[];
  reject_reasons: RejectReason[];
  honesty: string;
};
export type ReviewSettingsPatch = Partial<Omit<ReviewSettings, "reject_reasons" | "honesty">>;

export type Summary = { average: number | null; count: number; histogram: Record<"1" | "2" | "3" | "4" | "5", number> };
export type ReviewProduct = { id: string; product_id: string; name: string; image_url: string | null; url: string | null; kind: SubjectKind; summary: Summary; pending: number };

export type ReviewReport = {
  days: number;
  requests_sent: number;
  requests_answered: number;
  response_rate: number | null;
  reviews_written: number;
  average_rating: number | null;
  awaiting_moderation: number;
  by_status: Partial<Record<ReviewStatus, number>>;
  series: { date: string; reviews: number; average_rating: number | null }[];
  products: { product_id: string; name: string; reviews: number; average_rating: number | null; published: number; published_average: number | null }[];
};

export type ImportField = "product_id" | "product_handle" | "rating" | "title" | "body" | "name" | "email" | "date" | "verified" | "photos" | "reply";
export type ImportPreview = {
  dry_run: true;
  ready: boolean;
  headers: string[];
  mapping: Partial<Record<ImportField, string>>;
  fields: ImportField[];
  rows?: number;
  importable?: number;
  skipped?: number;
  errors?: { row: number; error: string }[];
  sample?: { product: string; rating: number; title: string; body: string; name: string; verified: boolean; photos: string[] }[];
  publish?: boolean;
  note: string;
};
export type ImportRun = { id: string; status: "queued" | "running" | "done" | "failed"; total: number; processed: number; imported: number; skipped: number; errors: { row?: number; error: string }[]; publish: boolean };
export type ImportInput = { csv: string; mapping?: Partial<Record<ImportField, string>>; publish?: boolean; confirm?: boolean };

/** Email block: a product's star summary and top published reviews. */
export type ReviewsBlock = { type: "reviews"; product_id?: string; count?: number; show?: "both" | "summary" | "reviews" };
/** Email block: ask for a review of the triggering event's items (or one product), with star links. */
export type ReviewRequestBlock = { type: "review_request"; product_id?: string; heading?: string; button_text?: string; show_stars?: boolean };

/** Segment rule: wrote a review (of a product, in a rating range, at least N), or has a review link waiting. */
export type ReviewCondition = {
  type: "review";
  action: "reviewed" | "pending_request";
  op: "has" | "has_not";
  product_id?: string;
  min_rating?: number;
  max_rating?: number;
  count?: number;
  within_days?: number;
};

/** Flow step: ask for reviews of the items in the triggering event, by email or text. */
export type ReviewRequestNode = {
  id: string;
  type: "review_request";
  name?: string;
  channel: "email" | "sms";
  /** Ask again only about this run's earlier links that are still unused. */
  reminder?: boolean;
  content: EmailContent | SmsContent;
  next: string | null;
};

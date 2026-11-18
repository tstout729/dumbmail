/** The account's sending review (`GET /api/screening`). */
export type ReviewStatus = "pending" | "approved" | "review" | "blocked";

export type ReviewAnswers = {
  business: string;
  list_source: string;
  monthly_volume: number;
};

export type ScreeningItem = {
  screening_id?: string;
  kind?: string;
  subject: "campaign" | "flow" | "import" | "domain" | null;
  subject_id: string | null;
  name: string | null;
  verdict: "pass" | "review" | "block";
  reasons: string[];
  by?: "person" | "automatic";
  created_at: string;
};

export type SendingReview = {
  enabled: boolean;
  status: ReviewStatus;
  can_send: boolean;
  since?: string | null;
  note?: string | null;
  message?: string;
  waiting_for?: "answers" | "sending_domain" | "check" | "person" | null;
  checking?: boolean;
  answers?: ReviewAnswers | null;
  held?: ScreeningItem[];
  recent?: ScreeningItem[];
};

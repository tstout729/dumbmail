import type { Align, EmailBlock, EmailContent, Template } from "../../api/types";

/** Show a block only when a rule about the person (or the event that started a flow) is true. */
export type RuleOp = "set" | "not_set" | "eq" | "neq" | "contains" | "gt" | "lt";
export type DisplayRule = { field: string; op: RuleOp; value?: string } | { expr: string };

/** Options every block can carry. */
export type BlockOptions = { show_if?: DisplayRule | null };

export type SocialNetwork = "instagram" | "facebook" | "tiktok" | "x" | "youtube" | "pinterest" | "linkedin" | "threads" | "website" | "email";
export type IconStyle = "color" | "dark" | "light" | "white";
export type SocialLink = { network: SocialNetwork; url: string };
export type NavLink = { text: string; href: string };
export type TableColumn = { label: string; align?: Align };

export type ContentBlock =
  | { type: "social"; links?: SocialLink[]; style?: IconStyle; size?: 24 | 32 | 40; align?: Align }
  | { type: "video"; url: string; thumbnail?: string; alt?: string; caption?: string }
  | { type: "countdown"; ends_at: string; background?: string; color?: string; label_color?: string; ended_text?: string }
  | { type: "table"; columns: TableColumn[]; rows?: string[][]; loop?: { source: string; cells: string[] } | null; footer?: string[][] }
  | { type: "quote"; text: string; author?: string; role?: string; image?: string; style?: "plain" | "card"; color?: string; align?: Align }
  | { type: "header"; logo?: string; logo_alt?: string; logo_width?: number; logo_href?: string; links?: NavLink[]; background?: string; link_color?: string }
  | { type: "nav"; links: NavLink[]; background?: string; color?: string; align?: Align; separator?: boolean }
  | { type: "footer"; text?: string; links?: NavLink[]; social?: SocialLink[]; show_social?: boolean; style?: IconStyle; align?: Align }
  | {
      type: "image_text";
      src: string;
      alt?: string;
      href?: string;
      heading?: string;
      html?: string;
      button_text?: string;
      button_href?: string;
      image_position?: "left" | "right";
      image_width?: 33 | 40 | 50;
      radius?: number;
    }
  | { type: "saved"; saved_block_id: string; name?: string; blocks: EmailBlock[]; frozen?: boolean };

export type SavedBlock = { id: string; name: string; blocks: EmailBlock[]; created_at: string; updated_at: string; uses?: number };
export type SavedBlockUse = { kind: "template" | "campaign" | "flow"; id: string; name: string; status: string | null; picks_up_changes: boolean };
export type SavedBlockDetail = SavedBlock & { used_in: SavedBlockUse[] };

export type Starter = { key: string; name: string; description: string; kind: "campaign" | "flow"; trigger: string | null; html?: string; subject?: string };
export type StarterDetail = Starter & { content: EmailContent; sample_event: unknown };

export type Severity = "error" | "warning" | "info";
export type CheckFinding = { id: string; severity: Severity; title: string; detail: string; blocks_sending: boolean };
export type CheckedLink = { url: string; kind: "link" | "image"; ok: boolean | null; status: number | null; note: string; bytes: number | null };
export type CheckResult = { ok: boolean; errors: number; warnings: number; findings: CheckFinding[]; size_bytes: number | null; recipients: number | null; links: CheckedLink[] };
export type CheckInput = { content?: EmailContent; campaign_id?: string; audience?: unknown; flow_id?: string; node_id?: string; skip_links?: boolean };

export type { Template };

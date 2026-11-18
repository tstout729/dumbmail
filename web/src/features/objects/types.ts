import type { EventCountOp, FieldFilter } from "../../api/types";

export type FieldKind = "text" | "number" | "boolean" | "date" | "datetime" | "enum" | "list" | "money";

export type FieldDef = {
  key: string;
  label?: string;
  type: FieldKind;
  /** enum: the allowed values. */
  options?: string[];
  /** money: e.g. USD. */
  currency?: string;
  /** Never in messages or lists, and hidden from people who can't read profiles. */
  sensitive?: boolean;
  /** A sensitive enum's options aren't shown; saving it without options keeps them. */
  options_hidden?: boolean;
};

export type ObjectType = {
  id: string;
  key: string;
  name: string;
  plural: string;
  fields: FieldDef[];
  title_field: string | null;
  created_at: string;
  updated_at: string;
  object_count?: number;
  used_by?: string[];
};

export type ObjectTypeInput = { key: string; name?: string; plural?: string; fields?: FieldDef[]; title_field?: string; convert?: boolean };

export type FieldValues = Record<string, unknown>;

export type ObjectSummary = {
  id: string;
  type: string;
  external_id: string;
  title: string | null;
  fields: FieldValues;
  version: number;
  created_at: string;
  updated_at: string;
};

export type ObjectChange = {
  id: string;
  kind: "created" | "updated";
  changed: string[];
  before: FieldValues;
  after: FieldValues;
  source: string | null;
  version: number;
  at: string;
};

export type LinkedProfile = { id: string; email: string | null; phone_number: string | null; first_name: string | null; last_name: string | null; relationship: string };

export type ObjectDetail = ObjectSummary & {
  type_name: string;
  hidden_fields: string[];
  source: string | null;
  as_of: string | null;
  history: ObjectChange[];
  linked_count: number;
  /** Only for people who can read profiles. */
  profiles?: LinkedProfile[];
};

export type ObjectPage = { data: ObjectSummary[]; total: number };

export type ProfileObjects = {
  types: { key: string; name: string; plural: string; fields: FieldDef[]; objects: { id: string; external_id: string; title: string | null; relationship: string; fields: FieldValues; updated_at: string }[] }[];
};

/** Segment rule: has (or, with `zero`, doesn't have) objects of a type whose fields match. */
export type ObjectCondition = { type: "object"; object_type: string; op: EventCountOp; count?: number; where?: FieldFilter[]; relationship?: string };

/** Flow trigger: an object of a type was created or updated. */
export type ObjectTrigger = {
  type: "object";
  object_type: string;
  on?: "created" | "updated" | "any";
  field?: string;
  to?: string | number | boolean;
  filters?: FieldFilter[];
  relationship?: string;
};

/** Flow trigger: N days (at an hour) or N hours before or after an object's date field. */
export type ObjectDateTrigger = {
  type: "object_date";
  object_type: string;
  field: string;
  offset_days?: number;
  offset_hours?: number;
  hour?: number;
  filters?: FieldFilter[];
  relationship?: string;
};

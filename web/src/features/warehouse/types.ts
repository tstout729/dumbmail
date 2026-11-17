export type DatasetInfo = { name: string; mode: "incremental" | "snapshot"; description: string; key: string[]; columns: string[] };

export type RunStatus = "queued" | "running" | "succeeded" | "failed" | "cancelled";

export type RunDataset = { name: string; mode: "incremental" | "snapshot"; rows: number; files: number; done: boolean; after: string | null; through: string };

export type Run = {
  id: string;
  destination_id: string;
  kind: "incremental" | "full";
  status: RunStatus;
  rows: number;
  files: number;
  bytes: number;
  attempts: number;
  error: string | null;
  manifest_key: string | null;
  window_end: string;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
  datasets: RunDataset[];
};

export type Schedule = "hourly" | "daily" | "manual";
export type Format = "jsonl" | "csv";

export type Destination = {
  id: string;
  /** Goes up when where or how files are written changes. */
  revision: number;
  name: string;
  endpoint: string | null;
  region: string;
  bucket: string;
  prefix: string;
  path_style: boolean;
  access_key_hint: string;
  format: Format;
  datasets: string[];
  schedule: Schedule;
  enabled: boolean;
  next_run_at: string | null;
  last_run_at: string | null;
  last_test_at: string | null;
  last_test_error: string | null;
  created_at: string;
  updated_at: string;
  last_run: Run | null;
};

export type DestinationInput = {
  /** The revision the editor read; a change made against an older one is refused. */
  revision?: number;
  name?: string;
  endpoint?: string;
  region?: string;
  bucket?: string;
  prefix?: string;
  access_key_id?: string;
  secret_access_key?: string;
  format?: Format;
  datasets?: string[];
  schedule?: Schedule;
  enabled?: boolean;
};

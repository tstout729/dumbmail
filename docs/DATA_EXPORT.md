# Data export

DumbMail writes your data to your own object storage bucket on a schedule, as gzip'd files your
data warehouse can load: Snowflake, BigQuery, Redshift, Databricks, DuckDB or anything that reads
from S3-compatible storage. Set it up in **Settings → Data export**, through the API
(`/api/warehouse/...`) or with the MCP tools.

There are no native warehouse connectors: the bucket is the hand-off, and the SQL below loads it.

## Destinations

A destination is a bucket on any S3-compatible service:

| Service | `endpoint` | `region` | Keys |
| --- | --- | --- | --- |
| AWS S3 | leave empty | the bucket's region, e.g. `us-east-1` | An IAM user's access key (policy below) |
| Cloudflare R2 | `https://<account id>.r2.cloudflarestorage.com` | `auto` | An R2 API token with Object Read & Write on the bucket |
| Google Cloud Storage | `https://storage.googleapis.com` | `auto` | An HMAC key for a service account with Storage Object Creator (Cloud Storage → Settings → Interoperability) |
| Backblaze B2 | `https://s3.<region>.backblazeb2.com` | e.g. `us-west-004` | An application key with `writeFiles` on the bucket |
| Other (MinIO, Wasabi, DigitalOcean Spaces...) | the service's S3 endpoint | the service's region | an S3 access key |

The endpoint must be `https://` on the public internet. `path_style` puts the bucket in the URL
path instead of the host name; it's on by default with a custom endpoint and off for AWS.

When you add a destination (or change its bucket or keys) DumbMail writes a small file under
`<prefix>_dumbmail_test/` and deletes it again; if the write fails, it tells you why and doesn't
save. A key that can't delete is fine. Keys are stored encrypted and never shown again, only a hint.

A minimal AWS IAM policy for the key:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": ["s3:PutObject", "s3:AbortMultipartUpload", "s3:DeleteObject"],
    "Resource": "arn:aws:s3:::your-bucket/dumbmail/*"
  }]
}
```

Up to 10 destinations per account. SFTP isn't supported.

## Schedules and runs

- `schedule`: `hourly` (at the top of each hour), `daily` (1am in the account's time zone, the
  default) or `manual`. The first scheduled run starts within a minute of saving.
- **Incremental runs** (scheduled, or "Export changes now") write each row that changed since the
  last successful run exactly once. Each incremental dataset keeps a watermark; a run covers
  `(watermark, run start − 5 minutes]`, so rows still being written when it starts are picked up by
  the next run instead of being missed. A failed run doesn't move the watermark, so the next run
  covers its window too.
- **Full exports** ("Full export", or `kind: "full"`) write every row of every dataset. Use one to
  backfill a new warehouse table. It also moves the watermarks forward.
- Only one run per destination at a time. A run writes files of up to 250,000 rows in bounded
  batches, streaming each to the bucket in 8 MB parts, and saves its place after every file. If the
  bucket fails, the run retries (1, 2, 4, 8 minutes) from the file it was on, rewriting that file
  under the same name. After 5 failed tries it stops and the account owner gets an email.
- The run log (Settings → Data export, or `GET /api/warehouse/destinations/:id/runs`) shows each
  run's status, rows, files and errors. Runs are kept for 90 days.

## File layout

```
<prefix><dataset>/dt=<YYYY-MM-DD>/run=<run id>/part-00000.jsonl.gz   (or .csv.gz)
<prefix>_manifests/dt=<YYYY-MM-DD>/run=<run id>.json
```

`dt` is the run's date in the account's time zone. `dt=` and `run=` are Hive-style partitions,
which BigQuery, Athena, Spark and DuckDB read as columns. A dataset with nothing new in a run has no
file for that run.

- **jsonl** (the default): one JSON object per line. `properties`, `definition`, `audience` and
  `trigger` are JSON objects.
- **csv**: a header row, then one row per record. JSON columns hold JSON text; empty means null.

Times are ISO 8601 with an offset (`2026-11-17T18:04:05.123456+00:00`). Ids are UUIDs.

### The manifest

Written last, after every file in the run, so its presence means the run is complete:

```json
{
  "run_id": "…", "account_id": "…", "destination_id": "…", "kind": "incremental",
  "format": "jsonl", "compression": "gzip", "schema_version": 1,
  "started_at": "…", "finished_at": "…", "window_end": "…", "date": "2026-11-17",
  "datasets": [
    {"name": "profiles", "mode": "incremental", "window": {"after": "…", "through": "…"}, "rows": 120,
     "files": [{"key": "dumbmail/profiles/dt=2026-11-17/run=…/part-00000.jsonl.gz", "rows": 120, "bytes": 18211}]},
    {"name": "lists", "mode": "snapshot", "window": null, "rows": 4, "files": [ … ]}
  ]
}
```

## Datasets

**Incremental** datasets have only what changed in the run's window: append them, and upsert
`profiles` by `id` (keep the row with the newest `updated_at`). **Snapshot** datasets are the whole
table every run: replace the warehouse table with the newest run's files.

| Dataset | Mode | Columns |
| --- | --- | --- |
| `profiles` | incremental (by `updated_at`) | `id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent, email_suppressed_reason, created_at, updated_at, last_event_at` |
| `events` | incremental (by `created_at`) | `id, profile_id, metric, properties, value, occurred_at, created_at, unique_id, message_id` |
| `messages` | incremental (by `created_at`) | `id, profile_id, channel, campaign_id, flow_id, flow_node_id, variant, to_address, subject, status, error, bounce_type, segments, cost_micros, sent_at, delivered_at, opened_at, clicked_at, unsubscribed_at, created_at` |
| `lists` | snapshot | `id, name, double_opt_in, show_on_preferences, description, member_count, created_at` |
| `list_members` | snapshot | `list_id, list, profile_id, added_at` |
| `segments` | snapshot | `id, name, definition, member_count, refreshed_at, created_at, updated_at` |
| `segment_members` | snapshot | `segment_id, segment, profile_id, entered_at` |
| `campaigns` | snapshot | `id, name, channel, status, subject, audience, send_at, sent_at, created_at, updated_at` |
| `flows` | snapshot | `id, name, status, trigger, created_at, updated_at` |
| `suppressions` | snapshot | `profile_id, email, phone_number, email_consent, sms_consent, email_suppressed_reason, updated_at` |

Notes:

- `email_consent` and `sms_consent` are `subscribed`, `unsubscribed`, `never_subscribed` (or
  `pending` while a double opt-in waits). `email_suppressed_reason` is `hard_bounce`, `complaint` or
  empty.
- `messages` rows show their outcome as of the export. Outcomes that come later (a later open or
  click, a bounce) are in `events` as `Opened Email`, `Clicked Email`, `Bounced Email`,
  `Marked Email as Spam`, `Unsubscribed` and so on, with the same `message_id`; a full export has
  every message's current status. `cost_micros` is the price in millionths of a dollar.
- `value` on events is the revenue for order events.
- `segment_members` is membership as of each segment's last refresh (about every 15 minutes).
- `GET /api/warehouse/datasets` lists the datasets and columns.

## Loading into a warehouse

The examples use the bucket `your-bucket`, the prefix `dumbmail/` and the `events` dataset in
JSON lines; the other datasets work the same way with their own columns.

### Snowflake

```sql
-- Once: a storage integration (ask your Snowflake admin) and a stage on the prefix.
CREATE STORAGE INTEGRATION dumbmail_s3
  TYPE = EXTERNAL_STAGE STORAGE_PROVIDER = 'S3' ENABLED = TRUE
  STORAGE_AWS_ROLE_ARN = 'arn:aws:iam::123456789012:role/snowflake-dumbmail'
  STORAGE_ALLOWED_LOCATIONS = ('s3://your-bucket/dumbmail/');
CREATE FILE FORMAT dumbmail_jsonl TYPE = JSON COMPRESSION = GZIP;
CREATE STAGE dumbmail_stage URL = 's3://your-bucket/dumbmail/' STORAGE_INTEGRATION = dumbmail_s3 FILE_FORMAT = dumbmail_jsonl;

CREATE TABLE IF NOT EXISTS dumbmail_events (
  id STRING, profile_id STRING, metric STRING, properties VARIANT, value FLOAT,
  occurred_at TIMESTAMP_TZ, created_at TIMESTAMP_TZ, unique_id STRING, message_id STRING
);

-- Run after each export. Snowflake remembers which files it loaded, so each file loads once.
COPY INTO dumbmail_events
FROM (
  SELECT $1:id::STRING, $1:profile_id::STRING, $1:metric::STRING, $1:properties, $1:value::FLOAT,
         $1:occurred_at::TIMESTAMP_TZ, $1:created_at::TIMESTAMP_TZ, $1:unique_id::STRING, $1:message_id::STRING
  FROM @dumbmail_stage/events/
)
PATTERN = '.*part-[0-9]+[.]jsonl[.]gz';

-- Profiles: load into a staging table the same way, then keep the newest version of each.
MERGE INTO dumbmail_profiles t
USING (SELECT * FROM dumbmail_profiles_staging QUALIFY ROW_NUMBER() OVER (PARTITION BY id ORDER BY updated_at DESC) = 1) s
ON t.id = s.id
WHEN MATCHED AND s.updated_at >= t.updated_at THEN UPDATE SET
  email = s.email, phone_number = s.phone_number, external_id = s.external_id, first_name = s.first_name, last_name = s.last_name,
  properties = s.properties, email_consent = s.email_consent, sms_consent = s.sms_consent,
  email_suppressed_reason = s.email_suppressed_reason, updated_at = s.updated_at, last_event_at = s.last_event_at
WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.email, s.phone_number, s.external_id, s.first_name, s.last_name, s.properties,
  s.email_consent, s.sms_consent, s.email_suppressed_reason, s.created_at, s.updated_at, s.last_event_at);
```

For a snapshot dataset, `TRUNCATE` the table and `COPY` with `PATTERN = '.*run=<run id from the manifest>/.*'`
(add `FORCE = TRUE`).

### BigQuery

With a Google Cloud Storage destination:

```sql
CREATE TABLE IF NOT EXISTS dumbmail.events (
  id STRING, profile_id STRING, metric STRING, properties JSON, value FLOAT64,
  occurred_at TIMESTAMP, created_at TIMESTAMP, unique_id STRING, message_id STRING
);

-- One day's files (gzip is detected from the file).
LOAD DATA INTO dumbmail.events
FROM FILES (format = 'JSON', uris = ['gs://your-bucket/dumbmail/events/dt=2026-11-17/*']);

-- Or query the bucket in place, with dt and run as columns.
CREATE OR REPLACE EXTERNAL TABLE dumbmail.events_files (
  id STRING, profile_id STRING, metric STRING, properties JSON, value FLOAT64,
  occurred_at TIMESTAMP, created_at TIMESTAMP, unique_id STRING, message_id STRING
)
WITH PARTITION COLUMNS (dt DATE, run STRING)
OPTIONS (
  format = 'NEWLINE_DELIMITED_JSON', compression = 'GZIP',
  uris = ['gs://your-bucket/dumbmail/events/*'],
  hive_partition_uri_prefix = 'gs://your-bucket/dumbmail/events'
);
```

From an S3 or R2 bucket, copy into Cloud Storage with the Storage Transfer Service first (or use
BigQuery Omni).

### Redshift

```sql
CREATE TABLE IF NOT EXISTS dumbmail_events (
  id VARCHAR(36), profile_id VARCHAR(36), metric VARCHAR(256), properties SUPER, value DOUBLE PRECISION,
  occurred_at TIMESTAMPTZ, created_at TIMESTAMPTZ, unique_id VARCHAR(512), message_id VARCHAR(36)
);

-- One day's files; JSON keys match the column names.
COPY dumbmail_events
FROM 's3://your-bucket/dumbmail/events/dt=2026-11-17/'
IAM_ROLE 'arn:aws:iam::123456789012:role/redshift-dumbmail'
FORMAT AS JSON 'auto'
GZIP
TIMEFORMAT 'auto';

-- CSV destinations:
-- COPY dumbmail_events FROM 's3://your-bucket/dumbmail/events/dt=2026-11-17/'
-- IAM_ROLE '…' FORMAT AS CSV IGNOREHEADER 1 GZIP TIMEFORMAT 'auto' EMPTYASNULL;
```

To load exactly the files of one run, list them in a Redshift manifest built from the DumbMail
manifest's `files[].key` (`{"entries": [{"url": "s3://your-bucket/<key>", "mandatory": true}]}`) and
add `MANIFEST` to the `COPY`.

## API

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/warehouse/datasets` | – | `{data: {name, mode, description, columns}[]}` |
| GET | `/api/warehouse/destinations` | – | `{data: Destination[], total}` |
| POST | `/api/warehouse/destinations` | `{name, region, bucket, access_key_id, secret_access_key, endpoint?, prefix?, path_style?, format?: "jsonl"\|"csv", datasets?, schedule?: "hourly"\|"daily"\|"manual", enabled?}` | `Destination`, 201. 400 with the storage service's error when the test write fails |
| GET/PATCH/DELETE | `/api/warehouse/destinations/:id` | partial body as above | `Destination`. Deleting keeps the files already written |
| POST | `/api/warehouse/destinations/:id/test` | – | `{ok, error?}` |
| POST | `/api/warehouse/destinations/:id/runs` | `{kind: "incremental"\|"full"}` | `Run`, 202. 409 while another run is going |
| GET | `/api/warehouse/destinations/:id/runs` | `?limit=` | `{data: Run[], total}`, newest first |
| GET | `/api/warehouse/runs/:id` | – | `Run` |

`Destination` is `{id, name, endpoint, region, bucket, prefix, path_style, access_key_hint, format,
datasets, schedule, enabled, next_run_at, last_run_at, last_test_at, last_test_error, created_at,
updated_at, last_run}`. `Run` is `{id, destination_id, kind, status: "queued"|"running"|"succeeded"|"failed",
rows, files, bytes, attempts, error, manifest_key, window_end, created_at, started_at, finished_at,
datasets: {name, mode, rows, files, done, after, through}[]}`.

## MCP tools

`list_export_datasets`, `list_export_destinations`, `create_export_destination`,
`update_export_destination`, `delete_export_destination` (needs `confirm: true`),
`test_export_destination`, `start_export` (a full export needs `confirm: true`), `list_export_runs`.

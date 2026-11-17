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
`<prefix>_dumbmail_test/` and deletes it again; if the write fails, it tells you why (the HTTP status
and the storage service's error code, such as `AccessDenied`) and doesn't save. A key that can't
delete is fine. Keys are stored encrypted and never shown again, only a hint, and they are only ever
sent to the host they were saved for: a change that sends requests somewhere else (a new endpoint,
region, or bucket in the host name) needs the keys again.

**Moving a destination.** Changing where or how files are written (the endpoint, region, bucket,
prefix, path style or format) cancels a run that's still going (status `cancelled`) and starts a
fresh baseline: the next run exports every row again, to the new place, so it has a complete copy.
Turning a destination back on after it was off does the same. A run always uses the settings the
destination had when the run started, so renaming it or changing its schedule mid-run changes
nothing about that run.

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
  default) or `manual`. A new (or moved) destination's first run starts about a minute after saving.
- **Exports are at-least-once, with stable keys.** That's what warehouses expect: every row carries
  its key (see `key` below) and `_updated_at`, and a row can appear again in a later run when it
  changed, or because a run re-reads a little of the previous one. Merge on the key and keep the
  copy with the latest `_updated_at` (SQL below); then reloading a file, or a repeat, changes nothing.
- **Incremental runs** (scheduled, or "Export changes now") write the rows that changed since the
  last successful run. Each incremental dataset keeps a watermark; a run covers `(watermark − 15
  minutes, run start]`. The 15-minute overlap catches rows whose transaction committed after the
  previous run read past their time. A failed run doesn't move the watermark, so the next run
  covers its window too.
- Changes are found two ways: by a row's own time column (`created_at` for new events and
  messages, `updated_at` for profiles) and, for changes that column doesn't show, by a change log
  DumbMail keeps for 7 days: a message's delivery, open, click, bounce, complaint or unsubscribe, and
  a profile's last activity. So a message exported as `sent` comes again with its later status. A
  destination that hasn't exported for more than 7 days starts again with a full baseline.
- **Full exports** ("Full export", or `kind: "full"`) write every row of every dataset. Use one to
  backfill a new warehouse table. It also moves the watermarks forward.
- Only one run per destination at a time, worked on by one server at a time. A run writes files of
  up to 250,000 rows (or 64 MB compressed). Each file is read in one consistent snapshot of the
  database, so a file never mixes rows from before and after a change (a dataset spread over several
  files is read file by file), then uploaded: one request for a small file, 8 MB parts for a big one.
  A file counts only once the storage service confirms the whole upload; anything else (an error, a
  timeout, an answer that doesn't say it worked) aborts it. The run saves its place after every
  file. If the bucket fails, the run retries (2, 4, 8, 16 minutes) from the file it was on,
  rewriting that file under the same name. After 5 failed tries it stops and the account owner gets
  an email (it says which destination and bucket, and the status and error code, nothing more).
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

Written last, after every file in the run is confirmed, so its presence means the run is complete.
The watermarks move only after the manifest is written, together with the run's final status:

```json
{
  "run_id": "…", "account_id": "…", "destination_id": "…", "kind": "incremental",
  "format": "jsonl", "compression": "gzip", "schema_version": 2,
  "started_at": "…", "finished_at": "…", "window_end": "…", "date": "2026-11-17",
  "datasets": [
    {"name": "profiles", "mode": "incremental", "key": ["id"], "window": {"after": "…", "through": "…"}, "rows": 120,
     "files": [{"key": "dumbmail/profiles/dt=2026-11-17/run=…/part-00000.jsonl.gz", "rows": 120, "bytes": 18211}]},
    {"name": "lists", "mode": "snapshot", "key": ["id"], "window": null, "rows": 4, "files": [ … ]}
  ]
}
```

## Datasets

**Incremental** datasets have what changed since the last run (and a little overlap): merge them
into the warehouse table on their key, keeping the copy with the latest `_updated_at`. **Snapshot**
datasets are the whole table every run: replace the warehouse table with the newest run's files
(rows that are gone, like someone who left a list, are simply not in them).

Every row also has `_updated_at`: the moment the row was read, so its values are as of then. Of two
copies of a row, the one with the later `_updated_at` is the newer one.

| Dataset | Mode | Key | Columns |
| --- | --- | --- | --- |
| `profiles` | incremental | `id` | `id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent, email_suppressed_reason, created_at, updated_at, last_event_at` |
| `events` | incremental | `id` | `id, profile_id, metric, properties, value, occurred_at, created_at, unique_id, message_id, from_browser` |
| `messages` | incremental | `id` | `id, profile_id, channel, campaign_id, flow_id, flow_node_id, variant, to_address, subject, status, error, bounce_type, segments, cost_micros, sent_at, delivered_at, opened_at, clicked_at, unsubscribed_at, created_at` |
| `lists` | snapshot | `id` | `id, name, double_opt_in, show_on_preferences, description, member_count, created_at` |
| `list_members` | snapshot | `list_id, profile_id` | `list_id, list, profile_id, added_at` |
| `segments` | snapshot | `id` | `id, name, definition, member_count, refreshed_at, created_at, updated_at` |
| `segment_members` | snapshot | `segment_id, profile_id` | `segment_id, segment, profile_id, entered_at` |
| `campaigns` | snapshot | `id` | `id, name, channel, status, subject, audience, send_at, sent_at, created_at, updated_at` |
| `flows` | snapshot | `id` | `id, name, status, trigger, created_at, updated_at` |
| `suppressions` | snapshot | `profile_id` | `profile_id, email, phone_number, email_consent, sms_consent, email_suppressed_reason, updated_at` |

CSV files have the columns in this order, then `_updated_at`.

Notes:

- `email_consent` and `sms_consent` are `subscribed`, `unsubscribed`, `never_subscribed` (or
  `pending` while a double opt-in waits). `email_suppressed_reason` is `hard_bounce`, `complaint` or
  empty.
- A `messages` row comes again in a later run when its outcome changes (delivered, opened, clicked,
  bounced, complained, unsubscribed), so after merging, each message has its latest status. The
  same outcomes are also in `events` (`Opened Email`, `Clicked Email`, `Bounced Email`,
  `Marked Email as Spam`, `Unsubscribed` ...) with the same `message_id`. `cost_micros` is the price
  in millionths of a dollar.
- `from_browser` on events is true for events a browser sent through the website snippet with the
  public site key. Anyone can send those with any values: don't count browser `Placed Order`
  events as revenue without checking them against your store.
- Deleted profiles aren't removed by an incremental export; a full export (or the snapshot of
  `suppressions`) shows who is still there.
- `value` on events is the revenue for order events.
- `segment_members` is membership as of each segment's last refresh (about every 15 minutes).
- `GET /api/warehouse/datasets` lists the datasets and columns.

## Loading into a warehouse

The examples use the bucket `your-bucket`, the prefix `dumbmail/` and JSON lines. Load each run's
files into a staging table, then merge on the dataset's key keeping the latest `_updated_at`: that
makes repeats and overlaps harmless. They show `events` and `messages`; the other incremental
dataset (`profiles`) works the same way with its own columns. For a snapshot dataset, replace the
table with one run's files instead.

### Snowflake

```sql
-- Once: a storage integration (ask your Snowflake admin) and a stage on the prefix.
CREATE STORAGE INTEGRATION dumbmail_s3
  TYPE = EXTERNAL_STAGE STORAGE_PROVIDER = 'S3' ENABLED = TRUE
  STORAGE_AWS_ROLE_ARN = 'arn:aws:iam::123456789012:role/snowflake-dumbmail'
  STORAGE_ALLOWED_LOCATIONS = ('s3://your-bucket/dumbmail/');
CREATE FILE FORMAT dumbmail_jsonl TYPE = JSON COMPRESSION = GZIP;
CREATE STAGE dumbmail_stage URL = 's3://your-bucket/dumbmail/' STORAGE_INTEGRATION = dumbmail_s3 FILE_FORMAT = dumbmail_jsonl;

CREATE TABLE IF NOT EXISTS dumbmail_messages (
  id STRING, profile_id STRING, channel STRING, campaign_id STRING, flow_id STRING, status STRING,
  sent_at TIMESTAMP_TZ, delivered_at TIMESTAMP_TZ, opened_at TIMESTAMP_TZ, clicked_at TIMESTAMP_TZ,
  created_at TIMESTAMP_TZ, _updated_at TIMESTAMP_TZ, raw VARIANT
);
CREATE TABLE IF NOT EXISTS dumbmail_messages_staging LIKE dumbmail_messages;

-- After each export: stage the new files (Snowflake remembers which files it loaded) ...
COPY INTO dumbmail_messages_staging
FROM (
  SELECT $1:id::STRING, $1:profile_id::STRING, $1:channel::STRING, $1:campaign_id::STRING, $1:flow_id::STRING, $1:status::STRING,
         $1:sent_at::TIMESTAMP_TZ, $1:delivered_at::TIMESTAMP_TZ, $1:opened_at::TIMESTAMP_TZ, $1:clicked_at::TIMESTAMP_TZ,
         $1:created_at::TIMESTAMP_TZ, $1:_updated_at::TIMESTAMP_TZ, $1
  FROM @dumbmail_stage/messages/
)
PATTERN = '.*part-[0-9]+[.]jsonl[.]gz';

-- ... then merge on the key, keeping the newest copy, and empty the staging table.
MERGE INTO dumbmail_messages t
USING (SELECT * FROM dumbmail_messages_staging QUALIFY ROW_NUMBER() OVER (PARTITION BY id ORDER BY _updated_at DESC) = 1) s
ON t.id = s.id
WHEN MATCHED AND s._updated_at > t._updated_at THEN UPDATE SET
  status = s.status, sent_at = s.sent_at, delivered_at = s.delivered_at, opened_at = s.opened_at, clicked_at = s.clicked_at,
  _updated_at = s._updated_at, raw = s.raw
WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.profile_id, s.channel, s.campaign_id, s.flow_id, s.status, s.sent_at, s.delivered_at,
  s.opened_at, s.clicked_at, s.created_at, s._updated_at, s.raw);
TRUNCATE dumbmail_messages_staging;
```

Events are never changed after they're recorded, so for `events` the merge only inserts what's new:
`MERGE INTO dumbmail_events t USING (… QUALIFY ROW_NUMBER() OVER (PARTITION BY id ORDER BY _updated_at DESC) = 1) s
ON t.id = s.id WHEN NOT MATCHED THEN INSERT …`.

For a snapshot dataset, `TRUNCATE` the table and `COPY` with `PATTERN = '.*run=<run id from the manifest>/.*'`
(add `FORCE = TRUE`).

### BigQuery

With a Google Cloud Storage destination, query the files in place (with `dt` and `run` as
columns) and merge them into a table:

```sql
CREATE OR REPLACE EXTERNAL TABLE dumbmail.messages_files (
  id STRING, profile_id STRING, channel STRING, campaign_id STRING, flow_id STRING, status STRING,
  sent_at TIMESTAMP, delivered_at TIMESTAMP, opened_at TIMESTAMP, clicked_at TIMESTAMP,
  created_at TIMESTAMP, _updated_at TIMESTAMP
)
WITH PARTITION COLUMNS (dt DATE, run STRING)
OPTIONS (
  format = 'NEWLINE_DELIMITED_JSON', compression = 'GZIP', ignore_unknown_values = true,
  uris = ['gs://your-bucket/dumbmail/messages/*'],
  hive_partition_uri_prefix = 'gs://your-bucket/dumbmail/messages'
);

CREATE TABLE IF NOT EXISTS dumbmail.messages AS SELECT * EXCEPT (dt, run) FROM dumbmail.messages_files WHERE FALSE;

-- After each export (here: the last two days of files, which covers the overlap).
MERGE dumbmail.messages t
USING (
  SELECT * EXCEPT (dt, run, rn) FROM (
    SELECT *, ROW_NUMBER() OVER (PARTITION BY id ORDER BY _updated_at DESC) AS rn
    FROM dumbmail.messages_files WHERE dt >= DATE_SUB(CURRENT_DATE(), INTERVAL 2 DAY)
  ) WHERE rn = 1
) s
ON t.id = s.id
WHEN MATCHED AND s._updated_at > t._updated_at THEN UPDATE SET
  status = s.status, sent_at = s.sent_at, delivered_at = s.delivered_at, opened_at = s.opened_at,
  clicked_at = s.clicked_at, _updated_at = s._updated_at
WHEN NOT MATCHED THEN INSERT ROW;
```

From an S3 or R2 bucket, copy into Cloud Storage with the Storage Transfer Service first (or use
BigQuery Omni).

### Redshift

```sql
CREATE TABLE IF NOT EXISTS dumbmail_events (
  id VARCHAR(36), profile_id VARCHAR(36), metric VARCHAR(256), properties SUPER, value DOUBLE PRECISION,
  occurred_at TIMESTAMPTZ, created_at TIMESTAMPTZ, unique_id VARCHAR(512), message_id VARCHAR(36),
  from_browser BOOLEAN, _updated_at TIMESTAMPTZ
);
CREATE TEMP TABLE dumbmail_events_staging (LIKE dumbmail_events);

-- One day's files into staging; JSON keys match the column names.
COPY dumbmail_events_staging
FROM 's3://your-bucket/dumbmail/events/dt=2026-11-17/'
IAM_ROLE 'arn:aws:iam::123456789012:role/redshift-dumbmail'
FORMAT AS JSON 'auto'
GZIP
TIMEFORMAT 'auto';

-- Keep one copy per key (the newest), then merge.
MERGE INTO dumbmail_events
USING (
  SELECT * FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY id ORDER BY _updated_at DESC) AS rn FROM dumbmail_events_staging) WHERE rn = 1
) s
ON dumbmail_events.id = s.id
WHEN MATCHED THEN UPDATE SET _updated_at = s._updated_at
WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.profile_id, s.metric, s.properties, s.value, s.occurred_at, s.created_at,
  s.unique_id, s.message_id, s.from_browser, s._updated_at);

-- CSV destinations:
-- COPY dumbmail_events_staging FROM 's3://your-bucket/dumbmail/events/dt=2026-11-17/'
-- IAM_ROLE '…' FORMAT AS CSV IGNOREHEADER 1 GZIP TIMEFORMAT 'auto' EMPTYASNULL;
```

For `messages` and `profiles`, set every changing column in the `WHEN MATCHED` branch (and only when
`s._updated_at > dumbmail_messages._updated_at`, by deleting older staging rows first if your
Redshift version doesn't allow a condition there).

To load exactly the files of one run, list them in a Redshift manifest built from the DumbMail
manifest's `files[].key` (`{"entries": [{"url": "s3://your-bucket/<key>", "mandatory": true}]}`) and
add `MANIFEST` to the `COPY`.

## API

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/warehouse/datasets` | – | `{data: {name, mode, description, key, columns}[]}` |
| GET | `/api/warehouse/destinations` | – | `{data: Destination[], total}` |
| POST | `/api/warehouse/destinations` | `{name, region, bucket, access_key_id, secret_access_key, endpoint?, prefix?, path_style?, format?: "jsonl"\|"csv", datasets?, schedule?: "hourly"\|"daily"\|"manual", enabled?}` | `Destination`, 201. 400 with the status and error code when the test write fails |
| GET/PATCH/DELETE | `/api/warehouse/destinations/:id` | partial body as above. A change that sends requests to another host needs both keys | `Destination`. Deleting keeps the files already written |
| POST | `/api/warehouse/destinations/:id/test` | – | `{ok, error?}` |
| POST | `/api/warehouse/destinations/:id/runs` | `{kind: "incremental"\|"full"}` | `Run`, 202. 409 while another run is going |
| GET | `/api/warehouse/destinations/:id/runs` | `?limit=` | `{data: Run[], total}`, newest first |
| GET | `/api/warehouse/runs/:id` | – | `Run` |

`Destination` is `{id, name, endpoint, region, bucket, prefix, path_style, access_key_hint, format,
datasets, schedule, enabled, next_run_at, last_run_at, last_test_at, last_test_error, created_at,
updated_at, last_run}`. `Run` is `{id, destination_id, kind, status: "queued"|"running"|"succeeded"|"failed"|"cancelled",
rows, files, bytes, attempts, error, manifest_key, window_end, created_at, started_at, finished_at,
datasets: {name, mode, rows, files, done, after, through}[]}`.

## MCP tools

`list_export_datasets`, `list_export_destinations`, `create_export_destination`,
`update_export_destination`, `delete_export_destination` (needs `confirm: true`),
`test_export_destination`, `start_warehouse_export` (a full export needs `confirm: true`), `list_export_runs`.

Who should be allowed (for the team roles that are coming): read-only roles may list datasets,
destinations (never keys) and runs; adding, changing, testing and deleting destinations and starting
exports are for admins and the owner.

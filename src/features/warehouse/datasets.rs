//! What each export dataset contains and how it is read in keyset order.
//!
//! Every query takes the same parameters: `$1` account id, `$2` window start (exclusive, NULL for
//! everything), `$3` window end (inclusive), `$4`/`$5` the cursor after the last row written (NULL
//! to start), `$6` rows to read. Each returns `(row jsonb, cursor_a text, cursor_b text)`.
//!
//! An incremental dataset is read in passes: `sql` finds rows by their own time column (new
//! events, new messages, profiles by `updated_at`), and `changes_sql`, when there is one, finds
//! rows whose other columns changed by the `warehouse_changes` log (a message's delivery, open or
//! bounce; a profile's last activity), with the same row filters. A row can come out of both, and
//! again in a later run: exports are at-least-once, keyed by `key`, and each row carries
//! `_updated_at`.
//!
//! A full read (a snapshot dataset, a full export, or an incremental dataset's first baseline)
//! instead takes every key once, in one statement (`stage`: `$1` account id, returning `(a, b)`
//! uuids), into `warehouse_stage`, and cuts files from there (`staged`: `$1` account id, `$2` run
//! id, `$3` dataset, `$4` the last `seq` written, `$5` rows to read; each returns
//! `(row jsonb, seq text, '')`). So a row added or moved behind a finished file's cursor can't be
//! skipped: the set is fixed when the dataset starts, and each row is read as it is when its file
//! is written.

pub struct Dataset {
    pub name: &'static str,
    /// `incremental`: rows that changed in the run's window. `snapshot`: everything, every run.
    pub incremental: bool,
    pub description: &'static str,
    /// The columns that identify a row (for MERGE / dedupe in the warehouse).
    pub key: &'static [&'static str],
    pub columns: &'static [&'static str],
    pub sql: &'static str,
    /// Rows changed after they were written, found by the change log (incremental runs only).
    pub changes_sql: Option<&'static str>,
    /// Every key, for a full read (see the module docs).
    pub stage: &'static str,
    /// Rows by their staged keys, in `seq` order.
    pub staged: &'static str,
}

impl Dataset {
    /// The queries a run reads in turn. A baseline (no window start) needs only the first.
    pub fn passes(&self, baseline: bool) -> Vec<&'static str> {
        match (self.changes_sql, baseline) {
            (Some(c), false) => vec![self.sql, c],
            _ => vec![self.sql],
        }
    }
}

/// Rows of `dataset` the change log saw change in the window. `$7` is the dataset name. `$filter`
/// is the base pass's row filter on `x`, so the change pass never adds rows the base pass leaves out.
macro_rules! change_pass {
    ($json:expr, $table:expr, $filter:expr) => {
        concat!(
            "SELECT ",
            $json,
            ", c.changed_at::text, c.row_id::text
             FROM warehouse_changes c JOIN ",
            $table,
            " x ON x.id = c.row_id AND x.account_id = $1",
            $filter,
            "
             WHERE c.account_id = $1 AND c.dataset = $7 AND c.changed_at > COALESCE($2, '-infinity') AND c.changed_at <= $3
               AND (c.changed_at, c.row_id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, '00000000-0000-0000-0000-000000000000'::uuid))
             ORDER BY c.changed_at, c.row_id LIMIT $6"
        )
    };
}

macro_rules! profile_json {
    () => {
        "jsonb_build_object('id', x.id, 'email', x.email, 'phone_number', x.phone_number, 'external_id', x.external_id, 'first_name', x.first_name,
            'last_name', x.last_name, 'properties', x.properties, 'email_consent', x.email_consent, 'sms_consent', x.sms_consent,
            'email_suppressed_reason', x.email_suppressed_reason, 'created_at', x.created_at, 'updated_at', x.updated_at, 'last_event_at', x.last_event_at)"
    };
}

macro_rules! message_json {
    () => {
        "jsonb_build_object('id', x.id, 'profile_id', x.profile_id, 'channel', x.channel, 'campaign_id', x.campaign_id, 'flow_id', x.flow_id,
            'flow_node_id', x.flow_node_id, 'variant', x.variant, 'to_address', x.to_address, 'subject', x.subject, 'status', x.status, 'error', x.error,
            'bounce_type', x.bounce_type, 'segments', x.segments, 'cost_micros', x.cost_micros, 'sent_at', x.sent_at, 'delivered_at', x.delivered_at,
            'opened_at', x.opened_at, 'clicked_at', x.clicked_at, 'unsubscribed_at', x.unsubscribed_at, 'created_at', x.created_at,
            'send_after', x.send_after, 'transactional', x.transactional,
            'transactional_send_id', (SELECT t.id FROM transactional_sends t WHERE t.message_id = x.id AND t.account_id = x.account_id LIMIT 1),
            'transactional_status', (SELECT t.status FROM transactional_sends t WHERE t.message_id = x.id AND t.account_id = x.account_id LIMIT 1))"
    };
}

/// Rows of `$table` (as `x`, keyed by `x.id`) by their staged keys.
macro_rules! staged_by_id {
    ($json:expr, $table:expr, $filter:expr) => {
        concat!(
            "SELECT ",
            $json,
            ", st.seq::text, ''::text
             FROM warehouse_stage st JOIN ",
            $table,
            " x ON x.id = st.key_a AND x.account_id = $1",
            $filter,
            "
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5"
        )
    };
}

/// Every key of `$table` (as `x`) in the account.
macro_rules! stage_ids {
    ($table:expr, $filter:expr) => {
        concat!("SELECT x.id AS a, NULL::uuid AS b FROM ", $table, " x WHERE x.account_id = $1", $filter)
    };
}

/// The lowest uuid: where a keyset cursor starts.
macro_rules! zero {
    () => {
        "'00000000-0000-0000-0000-000000000000'::uuid"
    };
}

pub const DATASETS: &[Dataset] = &[
    Dataset {
        name: "profiles",
        incremental: true,
        description: "Profiles with consent and custom properties. Incremental: profiles changed since the last run (their details, consent or last activity).",
        key: &["id"],
        columns: &["id", "email", "phone_number", "external_id", "first_name", "last_name", "properties", "email_consent", "sms_consent", "email_suppressed_reason", "created_at", "updated_at", "last_event_at"],
        sql: concat!(
            "SELECT ",
            profile_json!(),
            ", x.updated_at::text, x.id::text
             FROM profiles x
             WHERE x.account_id = $1 AND x.updated_at > COALESCE($2, '-infinity') AND x.updated_at <= $3 AND $7::text IS NOT DISTINCT FROM $7
               AND (x.updated_at, x.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY x.updated_at, x.id LIMIT $6"
        ),
        changes_sql: Some(change_pass!(profile_json!(), "profiles", "")),
        stage: stage_ids!("profiles", ""),
        staged: staged_by_id!(profile_json!(), "profiles", ""),
    },
    Dataset {
        name: "events",
        incremental: true,
        description: "Every tracked event (orders, site activity, message opens and clicks, sign-ups...). from_browser marks events a browser sent with the public site key. Incremental: events recorded since the last run (by created_at).",
        key: &["id"],
        columns: &["id", "profile_id", "metric", "properties", "value", "occurred_at", "created_at", "unique_id", "message_id", "from_browser"],
        changes_sql: None,
        sql: concat!(
            "SELECT jsonb_build_object('id', e.id, 'profile_id', e.profile_id, 'metric', m.name, 'properties', e.properties, 'value', e.value,
                'occurred_at', e.occurred_at, 'created_at', e.created_at, 'unique_id', e.unique_id, 'message_id', e.message_id, 'from_browser', e.from_browser),
                e.created_at::text, e.id::text
             FROM events e JOIN metrics m ON m.id = e.metric_id
             WHERE e.account_id = $1 AND e.created_at > COALESCE($2, '-infinity') AND e.created_at <= $3 AND $7::text IS NOT DISTINCT FROM $7
               AND (e.created_at, e.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY e.created_at, e.id LIMIT $6"
        ),
        stage: "SELECT e.id AS a, NULL::uuid AS b FROM events e WHERE e.account_id = $1",
        staged: "SELECT jsonb_build_object('id', e.id, 'profile_id', e.profile_id, 'metric', m.name, 'properties', e.properties, 'value', e.value,
                'occurred_at', e.occurred_at, 'created_at', e.created_at, 'unique_id', e.unique_id, 'message_id', e.message_id, 'from_browser', e.from_browser),
                st.seq::text, ''::text
             FROM warehouse_stage st JOIN events e ON e.id = st.key_a AND e.account_id = $1 JOIN metrics m ON m.id = e.metric_id
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
    Dataset {
        name: "messages",
        incremental: true,
        description: "Every email and text, campaign, flow and transactional, with its outcome (sent, delivered, opened, clicked, unsubscribed, bounced, skipped with the reason in error, failed). Incremental: messages created since the last run, and earlier ones whose outcome changed since then.",
        key: &["id"],
        columns: &["id", "profile_id", "channel", "campaign_id", "flow_id", "flow_node_id", "variant", "to_address", "subject", "status", "error", "bounce_type", "segments", "cost_micros", "sent_at", "delivered_at", "opened_at", "clicked_at", "unsubscribed_at", "created_at", "send_after", "transactional", "transactional_send_id", "transactional_status"],
        sql: concat!(
            "SELECT ",
            message_json!(),
            ", x.created_at::text, x.id::text
             FROM messages x
             WHERE x.account_id = $1 AND NOT x.is_test AND x.created_at > COALESCE($2, '-infinity') AND x.created_at <= $3 AND $7::text IS NOT DISTINCT FROM $7
               AND (x.created_at, x.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY x.created_at, x.id LIMIT $6"
        ),
        changes_sql: Some(change_pass!(message_json!(), "messages", " AND NOT x.is_test")),
        stage: stage_ids!("messages", " AND NOT x.is_test"),
        staged: staged_by_id!(message_json!(), "messages", " AND NOT x.is_test"),
    },
    Dataset {
        name: "lists",
        incremental: false,
        key: &["id"],
        changes_sql: None,
        description: "Every list. Snapshot: the whole table each run.",
        columns: &["id", "name", "double_opt_in", "show_on_preferences", "description", "member_count", "created_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', l.id, 'name', l.name, 'double_opt_in', l.double_opt_in, 'show_on_preferences', l.show_on_preferences,
                'description', l.description, 'member_count', (SELECT count(*) FROM list_members lm WHERE lm.list_id = l.id), 'created_at', l.created_at),
                l.id::text, ''
             FROM lists l WHERE l.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $7::text IS NOT DISTINCT FROM $7 AND $5::text IS NOT DISTINCT FROM $5
               AND l.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY l.id LIMIT $6"
        ),
        stage: "SELECT l.id AS a, NULL::uuid AS b FROM lists l WHERE l.account_id = $1",
        staged: "SELECT jsonb_build_object('id', l.id, 'name', l.name, 'double_opt_in', l.double_opt_in, 'show_on_preferences', l.show_on_preferences,
                'description', l.description, 'member_count', (SELECT count(*) FROM list_members lm WHERE lm.list_id = l.id), 'created_at', l.created_at),
                st.seq::text, ''::text
             FROM warehouse_stage st JOIN lists l ON l.id = st.key_a AND l.account_id = $1
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
    Dataset {
        name: "list_members",
        incremental: false,
        key: &["list_id", "profile_id"],
        changes_sql: None,
        description: "Who is on each list. Snapshot: every membership each run.",
        columns: &["list_id", "list", "profile_id", "added_at"],
        sql: concat!(
            "SELECT jsonb_build_object('list_id', l.id, 'list', l.name, 'profile_id', lm.profile_id, 'added_at', lm.added_at), l.id::text, lm.profile_id::text
             FROM (SELECT id, name FROM lists WHERE account_id = $1 AND id >= COALESCE($4::uuid, ",
            zero!(),
            ") AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $7::text IS NOT DISTINCT FROM $7) l
             CROSS JOIN LATERAL (SELECT x.profile_id, x.added_at FROM list_members x WHERE x.list_id = l.id
                 AND (l.id > COALESCE($4::uuid, ",
            zero!(),
            ") OR x.profile_id > COALESCE($5::uuid, ",
            zero!(),
            ")) ORDER BY x.profile_id LIMIT $6) lm
             ORDER BY l.id, lm.profile_id LIMIT $6"
        ),
        stage: "SELECT lm.list_id AS a, lm.profile_id AS b FROM list_members lm JOIN lists l ON l.id = lm.list_id WHERE l.account_id = $1",
        staged: "SELECT jsonb_build_object('list_id', l.id, 'list', l.name, 'profile_id', lm.profile_id, 'added_at', lm.added_at), st.seq::text, ''::text
             FROM warehouse_stage st JOIN lists l ON l.id = st.key_a AND l.account_id = $1 JOIN list_members lm ON lm.list_id = st.key_a AND lm.profile_id = st.key_b
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
    Dataset {
        name: "segments",
        incremental: false,
        key: &["id"],
        changes_sql: None,
        description: "Every segment with its rules and member count. Snapshot.",
        columns: &["id", "name", "definition", "member_count", "refreshed_at", "created_at", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', s.id, 'name', s.name, 'definition', s.definition, 'member_count', s.member_count, 'refreshed_at', s.refreshed_at,
                'created_at', s.created_at, 'updated_at', s.updated_at), s.id::text, ''
             FROM segments s WHERE s.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $7::text IS NOT DISTINCT FROM $7 AND $5::text IS NOT DISTINCT FROM $5
               AND s.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY s.id LIMIT $6"
        ),
        stage: "SELECT s.id AS a, NULL::uuid AS b FROM segments s WHERE s.account_id = $1",
        staged: "SELECT jsonb_build_object('id', s.id, 'name', s.name, 'definition', s.definition, 'member_count', s.member_count, 'refreshed_at', s.refreshed_at,
                'created_at', s.created_at, 'updated_at', s.updated_at), st.seq::text, ''::text
             FROM warehouse_stage st JOIN segments s ON s.id = st.key_a AND s.account_id = $1
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
    Dataset {
        name: "segment_members",
        incremental: false,
        key: &["segment_id", "profile_id"],
        changes_sql: None,
        description: "Who is in each segment as of its last refresh (segments refresh about every 15 minutes). Snapshot.",
        columns: &["segment_id", "segment", "profile_id", "entered_at"],
        sql: concat!(
            "SELECT jsonb_build_object('segment_id', s.id, 'segment', s.name, 'profile_id', sm.profile_id, 'entered_at', sm.entered_at), s.id::text, sm.profile_id::text
             FROM (SELECT id, name FROM segments WHERE account_id = $1 AND id >= COALESCE($4::uuid, ",
            zero!(),
            ") AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $7::text IS NOT DISTINCT FROM $7) s
             CROSS JOIN LATERAL (SELECT x.profile_id, x.entered_at FROM segment_members x WHERE x.segment_id = s.id
                 AND (s.id > COALESCE($4::uuid, ",
            zero!(),
            ") OR x.profile_id > COALESCE($5::uuid, ",
            zero!(),
            ")) ORDER BY x.profile_id LIMIT $6) sm
             ORDER BY s.id, sm.profile_id LIMIT $6"
        ),
        stage: "SELECT sm.segment_id AS a, sm.profile_id AS b FROM segment_members sm JOIN segments s ON s.id = sm.segment_id WHERE s.account_id = $1",
        staged: "SELECT jsonb_build_object('segment_id', s.id, 'segment', s.name, 'profile_id', sm.profile_id, 'entered_at', sm.entered_at), st.seq::text, ''::text
             FROM warehouse_stage st JOIN segments s ON s.id = st.key_a AND s.account_id = $1 JOIN segment_members sm ON sm.segment_id = st.key_a AND sm.profile_id = st.key_b
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
    Dataset {
        name: "campaigns",
        incremental: false,
        key: &["id"],
        changes_sql: None,
        description: "Every campaign: name, channel, status, subject, audience and send times. Snapshot.",
        columns: &["id", "name", "channel", "status", "subject", "audience", "send_at", "sent_at", "created_at", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', c.id, 'name', c.name, 'channel', c.channel, 'status', c.status, 'subject', c.content->>'subject',
                'audience', c.audience, 'send_at', c.send_at, 'sent_at', c.sent_at, 'created_at', c.created_at, 'updated_at', c.updated_at), c.id::text, ''
             FROM campaigns c WHERE c.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $7::text IS NOT DISTINCT FROM $7 AND $5::text IS NOT DISTINCT FROM $5
               AND c.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY c.id LIMIT $6"
        ),
        stage: "SELECT c.id AS a, NULL::uuid AS b FROM campaigns c WHERE c.account_id = $1",
        staged: "SELECT jsonb_build_object('id', c.id, 'name', c.name, 'channel', c.channel, 'status', c.status, 'subject', c.content->>'subject',
                'audience', c.audience, 'send_at', c.send_at, 'sent_at', c.sent_at, 'created_at', c.created_at, 'updated_at', c.updated_at), st.seq::text, ''::text
             FROM warehouse_stage st JOIN campaigns c ON c.id = st.key_a AND c.account_id = $1
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
    Dataset {
        name: "flows",
        incremental: false,
        key: &["id"],
        changes_sql: None,
        description: "Every flow: name, status and trigger. Snapshot.",
        columns: &["id", "name", "status", "trigger", "created_at", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', f.id, 'name', f.name, 'status', f.status, 'trigger', f.trigger, 'created_at', f.created_at, 'updated_at', f.updated_at),
                f.id::text, ''
             FROM flows f WHERE f.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $7::text IS NOT DISTINCT FROM $7 AND $5::text IS NOT DISTINCT FROM $5
               AND f.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY f.id LIMIT $6"
        ),
        stage: "SELECT f.id AS a, NULL::uuid AS b FROM flows f WHERE f.account_id = $1",
        staged: "SELECT jsonb_build_object('id', f.id, 'name', f.name, 'status', f.status, 'trigger', f.trigger, 'created_at', f.created_at, 'updated_at', f.updated_at),
                st.seq::text, ''::text
             FROM warehouse_stage st JOIN flows f ON f.id = st.key_a AND f.account_id = $1
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
    Dataset {
        name: "suppressions",
        incremental: false,
        key: &["profile_id"],
        changes_sql: None,
        description: "Everyone who unsubscribed from email or SMS, or whose email is suppressed after a hard bounce or spam complaint. Snapshot.",
        columns: &["profile_id", "email", "phone_number", "email_consent", "sms_consent", "email_suppressed_reason", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('profile_id', p.id, 'email', p.email, 'phone_number', p.phone_number, 'email_consent', p.email_consent,
                'sms_consent', p.sms_consent, 'email_suppressed_reason', p.email_suppressed_reason, 'updated_at', p.updated_at), p.created_at::text, p.id::text
             FROM profiles p
             WHERE p.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $7::text IS NOT DISTINCT FROM $7
               AND (p.email_consent = 'unsubscribed' OR p.sms_consent = 'unsubscribed' OR p.email_suppressed_reason IS NOT NULL)
               AND (p.created_at, p.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY p.created_at, p.id LIMIT $6"
        ),
        stage: "SELECT p.id AS a, NULL::uuid AS b FROM profiles p
             WHERE p.account_id = $1 AND (p.email_consent = 'unsubscribed' OR p.sms_consent = 'unsubscribed' OR p.email_suppressed_reason IS NOT NULL)",
        staged: "SELECT jsonb_build_object('profile_id', p.id, 'email', p.email, 'phone_number', p.phone_number, 'email_consent', p.email_consent,
                'sms_consent', p.sms_consent, 'email_suppressed_reason', p.email_suppressed_reason, 'updated_at', p.updated_at), st.seq::text, ''::text
             FROM warehouse_stage st JOIN profiles p ON p.id = st.key_a AND p.account_id = $1
             WHERE st.run_id = $2 AND st.dataset = $3 AND st.seq > $4
             ORDER BY st.seq LIMIT $5",
    },
];

pub fn find(name: &str) -> Option<&'static Dataset> {
    DATASETS.iter().find(|d| d.name == name)
}

pub fn all_names() -> Vec<String> {
    DATASETS.iter().map(|d| d.name.to_string()).collect()
}

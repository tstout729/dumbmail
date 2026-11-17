//! What each export dataset contains and how it is read in keyset order.
//!
//! Every query takes the same parameters: `$1` account id, `$2` window start (exclusive, NULL for
//! everything), `$3` window end (inclusive), `$4`/`$5` the cursor after the last row written (NULL
//! to start), `$6` rows to read. Each returns `(row jsonb, cursor_a text, cursor_b text)`.

pub struct Dataset {
    pub name: &'static str,
    /// `incremental`: rows that changed in the run's window. `snapshot`: everything, every run.
    pub incremental: bool,
    pub description: &'static str,
    pub columns: &'static [&'static str],
    pub sql: &'static str,
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
        description: "Profiles with consent and custom properties. Incremental: profiles changed since the last run (by updated_at).",
        columns: &["id", "email", "phone_number", "external_id", "first_name", "last_name", "properties", "email_consent", "sms_consent", "email_suppressed_reason", "created_at", "updated_at", "last_event_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', p.id, 'email', p.email, 'phone_number', p.phone_number, 'external_id', p.external_id, 'first_name', p.first_name,
                'last_name', p.last_name, 'properties', p.properties, 'email_consent', p.email_consent, 'sms_consent', p.sms_consent,
                'email_suppressed_reason', p.email_suppressed_reason, 'created_at', p.created_at, 'updated_at', p.updated_at, 'last_event_at', p.last_event_at),
                p.updated_at::text, p.id::text
             FROM profiles p
             WHERE p.account_id = $1 AND p.updated_at > COALESCE($2, '-infinity') AND p.updated_at <= $3
               AND (p.updated_at, p.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY p.updated_at, p.id LIMIT $6"
        ),
    },
    Dataset {
        name: "events",
        incremental: true,
        description: "Every tracked event (orders, site activity, message opens and clicks, sign-ups...). Incremental: events recorded since the last run (by created_at).",
        columns: &["id", "profile_id", "metric", "properties", "value", "occurred_at", "created_at", "unique_id", "message_id"],
        sql: concat!(
            "SELECT jsonb_build_object('id', e.id, 'profile_id', e.profile_id, 'metric', m.name, 'properties', e.properties, 'value', e.value,
                'occurred_at', e.occurred_at, 'created_at', e.created_at, 'unique_id', e.unique_id, 'message_id', e.message_id),
                e.created_at::text, e.id::text
             FROM events e JOIN metrics m ON m.id = e.metric_id
             WHERE e.account_id = $1 AND e.created_at > COALESCE($2, '-infinity') AND e.created_at <= $3
               AND (e.created_at, e.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY e.created_at, e.id LIMIT $6"
        ),
    },
    Dataset {
        name: "messages",
        incremental: true,
        description: "Every email and text sent, with its outcome when exported (status, delivered, opened, clicked, unsubscribed). Incremental: messages created since the last run. Later outcomes also arrive as events (Opened Email, Clicked Email, Bounced Email...) with message_id.",
        columns: &["id", "profile_id", "channel", "campaign_id", "flow_id", "flow_node_id", "variant", "to_address", "subject", "status", "error", "bounce_type", "segments", "cost_micros", "sent_at", "delivered_at", "opened_at", "clicked_at", "unsubscribed_at", "created_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', x.id, 'profile_id', x.profile_id, 'channel', x.channel, 'campaign_id', x.campaign_id, 'flow_id', x.flow_id,
                'flow_node_id', x.flow_node_id, 'variant', x.variant, 'to_address', x.to_address, 'subject', x.subject, 'status', x.status, 'error', x.error,
                'bounce_type', x.bounce_type, 'segments', x.segments, 'cost_micros', x.cost_micros, 'sent_at', x.sent_at, 'delivered_at', x.delivered_at,
                'opened_at', x.opened_at, 'clicked_at', x.clicked_at, 'unsubscribed_at', x.unsubscribed_at, 'created_at', x.created_at),
                x.created_at::text, x.id::text
             FROM messages x
             WHERE x.account_id = $1 AND NOT x.is_test AND x.created_at > COALESCE($2, '-infinity') AND x.created_at <= $3
               AND (x.created_at, x.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY x.created_at, x.id LIMIT $6"
        ),
    },
    Dataset {
        name: "lists",
        incremental: false,
        description: "Every list. Snapshot: the whole table each run.",
        columns: &["id", "name", "double_opt_in", "show_on_preferences", "description", "member_count", "created_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', l.id, 'name', l.name, 'double_opt_in', l.double_opt_in, 'show_on_preferences', l.show_on_preferences,
                'description', l.description, 'member_count', (SELECT count(*) FROM list_members lm WHERE lm.list_id = l.id), 'created_at', l.created_at),
                l.id::text, ''
             FROM lists l WHERE l.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $5::text IS NOT DISTINCT FROM $5
               AND l.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY l.id LIMIT $6"
        ),
    },
    Dataset {
        name: "list_members",
        incremental: false,
        description: "Who is on each list. Snapshot: every membership each run.",
        columns: &["list_id", "list", "profile_id", "added_at"],
        sql: concat!(
            "SELECT jsonb_build_object('list_id', l.id, 'list', l.name, 'profile_id', lm.profile_id, 'added_at', lm.added_at), l.id::text, lm.profile_id::text
             FROM (SELECT id, name FROM lists WHERE account_id = $1 AND id >= COALESCE($4::uuid, ",
            zero!(),
            ") AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3) l
             CROSS JOIN LATERAL (SELECT x.profile_id, x.added_at FROM list_members x WHERE x.list_id = l.id
                 AND (l.id > COALESCE($4::uuid, ",
            zero!(),
            ") OR x.profile_id > COALESCE($5::uuid, ",
            zero!(),
            ")) ORDER BY x.profile_id LIMIT $6) lm
             ORDER BY l.id, lm.profile_id LIMIT $6"
        ),
    },
    Dataset {
        name: "segments",
        incremental: false,
        description: "Every segment with its rules and member count. Snapshot.",
        columns: &["id", "name", "definition", "member_count", "refreshed_at", "created_at", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', s.id, 'name', s.name, 'definition', s.definition, 'member_count', s.member_count, 'refreshed_at', s.refreshed_at,
                'created_at', s.created_at, 'updated_at', s.updated_at), s.id::text, ''
             FROM segments s WHERE s.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $5::text IS NOT DISTINCT FROM $5
               AND s.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY s.id LIMIT $6"
        ),
    },
    Dataset {
        name: "segment_members",
        incremental: false,
        description: "Who is in each segment as of its last refresh (segments refresh about every 15 minutes). Snapshot.",
        columns: &["segment_id", "segment", "profile_id", "entered_at"],
        sql: concat!(
            "SELECT jsonb_build_object('segment_id', s.id, 'segment', s.name, 'profile_id', sm.profile_id, 'entered_at', sm.entered_at), s.id::text, sm.profile_id::text
             FROM (SELECT id, name FROM segments WHERE account_id = $1 AND id >= COALESCE($4::uuid, ",
            zero!(),
            ") AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3) s
             CROSS JOIN LATERAL (SELECT x.profile_id, x.entered_at FROM segment_members x WHERE x.segment_id = s.id
                 AND (s.id > COALESCE($4::uuid, ",
            zero!(),
            ") OR x.profile_id > COALESCE($5::uuid, ",
            zero!(),
            ")) ORDER BY x.profile_id LIMIT $6) sm
             ORDER BY s.id, sm.profile_id LIMIT $6"
        ),
    },
    Dataset {
        name: "campaigns",
        incremental: false,
        description: "Every campaign: name, channel, status, subject, audience and send times. Snapshot.",
        columns: &["id", "name", "channel", "status", "subject", "audience", "send_at", "sent_at", "created_at", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', c.id, 'name', c.name, 'channel', c.channel, 'status', c.status, 'subject', c.content->>'subject',
                'audience', c.audience, 'send_at', c.send_at, 'sent_at', c.sent_at, 'created_at', c.created_at, 'updated_at', c.updated_at), c.id::text, ''
             FROM campaigns c WHERE c.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $5::text IS NOT DISTINCT FROM $5
               AND c.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY c.id LIMIT $6"
        ),
    },
    Dataset {
        name: "flows",
        incremental: false,
        description: "Every flow: name, status and trigger. Snapshot.",
        columns: &["id", "name", "status", "trigger", "created_at", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('id', f.id, 'name', f.name, 'status', f.status, 'trigger', f.trigger, 'created_at', f.created_at, 'updated_at', f.updated_at),
                f.id::text, ''
             FROM flows f WHERE f.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3 AND $5::text IS NOT DISTINCT FROM $5
               AND f.id > COALESCE($4::uuid, ",
            zero!(),
            ") ORDER BY f.id LIMIT $6"
        ),
    },
    Dataset {
        name: "suppressions",
        incremental: false,
        description: "Everyone who unsubscribed from email or SMS, or whose email is suppressed after a hard bounce or spam complaint. Snapshot.",
        columns: &["profile_id", "email", "phone_number", "email_consent", "sms_consent", "email_suppressed_reason", "updated_at"],
        sql: concat!(
            "SELECT jsonb_build_object('profile_id', p.id, 'email', p.email, 'phone_number', p.phone_number, 'email_consent', p.email_consent,
                'sms_consent', p.sms_consent, 'email_suppressed_reason', p.email_suppressed_reason, 'updated_at', p.updated_at), p.created_at::text, p.id::text
             FROM profiles p
             WHERE p.account_id = $1 AND $2::timestamptz IS NOT DISTINCT FROM $2 AND $3::timestamptz IS NOT DISTINCT FROM $3
               AND (p.email_consent = 'unsubscribed' OR p.sms_consent = 'unsubscribed' OR p.email_suppressed_reason IS NOT NULL)
               AND (p.created_at, p.id) > (COALESCE($4::timestamptz, '-infinity'), COALESCE($5::uuid, ",
            zero!(),
            "))
             ORDER BY p.created_at, p.id LIMIT $6"
        ),
    },
];

pub fn find(name: &str) -> Option<&'static Dataset> {
    DATASETS.iter().find(|d| d.name == name)
}

pub fn all_names() -> Vec<String> {
    DATASETS.iter().map(|d| d.name.to_string()).collect()
}

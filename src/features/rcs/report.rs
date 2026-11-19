//! RCS and SMS side by side for a campaign or a flow: sent, delivered, read (RCS only) and clicked.
//! A text counts as RCS when it ended on the RCS route; a fallback text counts as SMS. The totals
//! are the campaign's or flow's usual stats.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

type Row = (bool, i64, i64, i64, i64);

fn side(rows: &[Row], rcs: bool) -> Value {
    let r = rows.iter().find(|r| r.0 == rcs).copied().unwrap_or((rcs, 0, 0, 0, 0));
    let mut v = json!({"sent": r.1, "delivered": r.2, "clicked": r.4});
    if rcs {
        v["read"] = json!(r.3);
    }
    v
}

pub async fn report(state: &AppState, account_id: Uuid, campaign_id: Option<Uuid>, flow_id: Option<Uuid>) -> AppResult<Value> {
    let (column, id) = match (campaign_id, flow_id) {
        (Some(c), None) => ("campaign_id", c),
        (None, Some(f)) => ("flow_id", f),
        _ => return Err(AppError::bad("give campaign_id or flow_id")),
    };
    // Two separate fixed queries keep the column name out of user input.
    let sql = format!(
        "SELECT COALESCE(r.route = 'rcs', false), count(*) FILTER (WHERE m.status IN ('sent', 'delivered'))::bigint, count(*) FILTER (WHERE m.status = 'delivered')::bigint,
                count(r.read_at) FILTER (WHERE r.route = 'rcs')::bigint, count(m.clicked_at)::bigint
         FROM messages m LEFT JOIN rcs_messages r ON r.message_id = m.id
         WHERE m.account_id = $1 AND m.{column} = $2 AND m.channel = 'sms' AND NOT m.is_test AND m.status <> 'skipped'
         GROUP BY 1"
    );
    let rows: Vec<Row> = sqlx::query_as(&sql).bind(account_id).bind(id).fetch_all(&state.db).await?;
    Ok(json!({"rcs": side(&rows, true), "sms": side(&rows, false), "any_rcs": rows.iter().any(|r| r.0)}))
}

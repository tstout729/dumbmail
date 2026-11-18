//! Two starter flows a store gets when it first connects: "Order confirmed" and "Order shipped".
//! Plain order updates with no marketing content, marked as service messages (so they reach
//! people held back from marketing, at the address the store supplied), and created as drafts:
//! they never go live until the merchant turns them on.

use serde_json::{json, Value};
use uuid::Uuid;

fn order_email(subject: &str, heading: &str, body: &str) -> Value {
    json!({"id": "e1", "type": "email", "name": subject, "next": null, "content": {
        "subject": subject, "preheader": "{{ event.properties.order_name }}",
        "blocks": [
            {"type": "heading", "text": heading, "align": "left"},
            {"type": "text", "html": body},
            {"type": "products", "source": "event_items", "count": 6, "columns": 2, "button_text": "View", "hide_out_of_stock": false},
            {"type": "button", "text": "View your order", "href": "{{ event.properties.order_status_url|default(organization.website) }}"}
        ]}})
}

/// The two starters: (name, trigger, definition).
pub fn definitions() -> Vec<(&'static str, Value, Value)> {
    let settings = json!({"service_messages": true, "reentry": "always"});
    vec![
        (
            "Order confirmed",
            json!({"type": "metric", "metric": "Placed Order"}),
            json!({"entry": "e1", "settings": settings, "nodes": [order_email("Your order {{ event.properties.order_name }} is confirmed", "Thanks for your order",
                "<p>We've got your order {{ event.properties.order_name }} and we're getting it ready. We'll let you know when it ships.</p>")]}),
        ),
        (
            "Order shipped",
            json!({"type": "metric", "metric": "Fulfilled Order"}),
            json!({"entry": "e1", "settings": settings, "nodes": [order_email("Your order {{ event.properties.order_name }} has shipped", "Your order is on its way",
                "<p>Your order {{ event.properties.order_name }} has shipped.</p>")]}),
        ),
    ]
}

/// Create the starters as drafts, once per account (a flow of the same name is left alone).
pub async fn ensure(db: &sqlx::PgPool, account_id: Uuid) -> anyhow::Result<()> {
    for (name, trigger, definition) in definitions() {
        crate::flows::parse_trigger(&trigger).map_err(|e| anyhow::anyhow!(e.public_message()))?;
        crate::flows::parse_definition(&definition).map_err(|e| anyhow::anyhow!(e.public_message()))?;
        sqlx::query(
            "INSERT INTO flows (id, account_id, name, status, trigger, definition)
             SELECT $1, $2, $3, 'draft', $4, $5 WHERE NOT EXISTS (SELECT 1 FROM flows WHERE account_id = $2 AND name = $3)",
        )
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(name)
        .bind(&trigger)
        .bind(&definition)
        .execute(db)
        .await?;
    }
    Ok(())
}

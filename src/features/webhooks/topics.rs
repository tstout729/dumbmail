//! Webhook topics: what each one means, and a sample of its `data`.

use serde_json::{json, Value};

pub struct Topic {
    pub name: &'static str,
    pub group: &'static str,
    pub description: &'static str,
}

pub const TOPICS: &[Topic] = &[
    Topic { name: "event.tracked", group: "Events", description: "Any event is recorded (API, website, forms, messages). Pick metrics to get only those. The envelope's source says who recorded it." },
    Topic { name: "profile.created", group: "Profiles", description: "A new profile is added." },
    Topic { name: "profile.updated", group: "Profiles", description: "A profile's details, properties or consent change." },
    Topic { name: "profile.deleted", group: "Profiles", description: "A profile is deleted." },
    Topic { name: "profile.subscribed", group: "Consent", description: "Someone subscribes to email or SMS." },
    Topic { name: "profile.unsubscribed", group: "Consent", description: "Someone unsubscribes from email or SMS, with the reason." },
    Topic { name: "profile.suppressed", group: "Consent", description: "An address is suppressed after a hard bounce or a spam complaint." },
    Topic { name: "list.member_added", group: "Lists and segments", description: "A profile joins a list." },
    Topic { name: "list.member_removed", group: "Lists and segments", description: "A profile leaves a list." },
    Topic { name: "segment.entered", group: "Lists and segments", description: "A profile enters a segment (checked when segments refresh, about every 15 minutes)." },
    Topic { name: "segment.left", group: "Lists and segments", description: "A profile leaves a segment (checked when segments refresh)." },
    Topic { name: "message.sent", group: "Messages", description: "An email or text is sent." },
    Topic { name: "message.skipped", group: "Messages", description: "A message isn't sent, with the reason (smart sending, quiet hours, a cancelled campaign, an unsubscribe ...)." },
    Topic { name: "message.failed", group: "Messages", description: "A message couldn't be sent, with the reason." },
    Topic { name: "message.delivered", group: "Messages", description: "The provider confirms delivery." },
    Topic { name: "message.opened", group: "Messages", description: "An email is opened (first open only)." },
    Topic { name: "message.clicked", group: "Messages", description: "A link in an email or text is clicked." },
    Topic { name: "message.bounced", group: "Messages", description: "An email bounces." },
    Topic { name: "message.complained", group: "Messages", description: "Someone marks an email as spam." },
    Topic { name: "form.submitted", group: "Forms", description: "Someone signs up through a form." },
    Topic { name: "campaign.sent", group: "Campaigns and flows", description: "A campaign starts sending." },
    Topic { name: "campaign.finished", group: "Campaigns and flows", description: "A campaign has finished sending." },
    Topic { name: "flow.status_changed", group: "Campaigns and flows", description: "A flow is turned on, paused or set back to draft." },
    Topic { name: "import.finished", group: "Campaigns and flows", description: "A profile import finishes." },
];

pub fn is_topic(name: &str) -> bool {
    TOPICS.iter().any(|t| t.name == name)
}

pub fn list_json() -> Value {
    Value::Array(TOPICS.iter().map(|t| json!({"topic": t.name, "group": t.group, "description": t.description})).collect())
}

/// The message topic one of DumbMail's own message events stands for (besides `event.tracked`).
pub fn message_topic(metric: &str) -> Option<&'static str> {
    Some(match metric {
        "Received Email" | "Received SMS" | crate::features::transactional::send::METRIC => "message.sent",
        "Opened Email" => "message.opened",
        "Clicked Email" | "Clicked SMS" => "message.clicked",
        "Bounced Email" => "message.bounced",
        "Marked Email as Spam" => "message.complained",
        _ => return None,
    })
}

fn sample_profile() -> Value {
    json!({"id": "0193a5b4-6f1e-7c3a-9d2e-1b2c3d4e5f60", "email": "jane@example.com", "phone_number": "+15035550100", "external_id": "cust_1042", "first_name": "Jane", "last_name": "Doe"})
}

fn sample_message() -> Value {
    json!({"id": "0193a5b4-7a10-7e55-8c11-2f3a4b5c6d70", "channel": "email", "subject": "New arrivals are here", "status": "sent",
        "campaign_id": "0193a5b4-7b20-7f66-9d22-3a4b5c6d7e80", "flow_id": null, "flow_node_id": null, "sent_at": "2026-11-17T18:04:05Z",
        "delivered_at": null, "send_after": null, "transactional": false, "transactional_send": null})
}

/// Sample `data` for a topic, shaped exactly like the real thing.
pub fn sample_data(topic: &str, metric: Option<&str>) -> Value {
    let profile = sample_profile();
    let full_profile = json!({"id": profile["id"], "email": "jane@example.com", "phone_number": "+15035550100", "external_id": "cust_1042",
        "first_name": "Jane", "last_name": "Doe", "properties": {"city": "Portland", "vip": true}, "email_consent": "subscribed", "sms_consent": "never_subscribed",
        "created_at": "2026-10-02T16:20:00Z", "updated_at": "2026-11-17T18:04:05Z", "last_event_at": "2026-11-17T18:04:05Z"});
    match topic {
        "event.tracked" => json!({
            "event": {"id": "0193a5b4-8c30-7a77-8e33-4b5c6d7e8f90", "metric": metric.unwrap_or("Placed Order"), "properties": {"order_id": "1042", "items": [{"name": "Canvas tote", "price": 38.0}]},
                "value": 38.0, "occurred_at": "2026-11-17T18:04:05Z", "unique_id": "1042", "message_id": null},
            "profile": profile,
        }),
        "profile.created" | "profile.updated" => {
            let mut d = json!({"profile": full_profile});
            if topic == "profile.updated" {
                d["changed"] = json!(["first_name", "properties"]);
            }
            d
        }
        "profile.deleted" => json!({"profile": profile}),
        "profile.subscribed" => json!({"channel": "email", "reason": "double_opt_in", "before": "pending", "after": "subscribed", "profile": profile}),
        "profile.unsubscribed" => json!({"channel": "email", "reason": "unsubscribe_link", "before": "subscribed", "after": "unsubscribed", "message_id": sample_message()["id"], "profile": profile}),
        "profile.suppressed" => json!({"channel": "email", "reason": "hard_bounce", "message_id": sample_message()["id"], "profile": profile}),
        "list.member_added" | "list.member_removed" => json!({"list": {"id": "0193a5b4-9d40-7b88-9f44-5c6d7e8f9a01", "name": "Newsletter"}, "profile": profile}),
        "segment.entered" | "segment.left" => json!({"segment": {"id": "0193a5b4-ae50-7c99-8a55-6d7e8f9a0b12", "name": "VIP customers"}, "profile": profile}),
        "message.sent" | "message.delivered" | "message.opened" | "message.complained" => json!({"message": sample_message(), "profile": profile}),
        "message.clicked" => json!({"message": sample_message(), "url": "https://example.com/new", "profile": profile}),
        "message.bounced" => json!({"message": sample_message(), "bounce_type": "Permanent", "profile": profile}),
        "message.skipped" => {
            let mut m = sample_message();
            m["status"] = json!("skipped");
            m["sent_at"] = Value::Null;
            json!({"message": m, "reason": "smart sending", "profile": profile})
        }
        "message.failed" => {
            let mut m = sample_message();
            m["status"] = json!("failed");
            m["sent_at"] = Value::Null;
            json!({"message": m, "reason": "not sent: email sending is paused for this account", "profile": profile})
        }
        "form.submitted" => json!({"form": {"id": "0193a5b4-bf60-7daa-9b66-7e8f9a0b1c23", "name": "Welcome popup", "kind": "popup"}, "list_id": "0193a5b4-9d40-7b88-9f44-5c6d7e8f9a01",
            "variant": "a", "profile": profile}),
        "campaign.sent" | "campaign.finished" => json!({"campaign": {"id": "0193a5b4-7b20-7f66-9d22-3a4b5c6d7e80", "name": "November launch", "channel": "email",
            "status": if topic == "campaign.sent" { "sending" } else { "sent" }, "sent_at": "2026-11-17T18:00:00Z", "recipients": 12840}}),
        "flow.status_changed" => json!({"flow": {"id": "0193a5b4-c070-7ebb-8c77-8f9a0b1c2d34", "name": "Welcome series", "status": "live", "previous_status": "draft",
            "trigger": {"type": "list", "list_id": "0193a5b4-9d40-7b88-9f44-5c6d7e8f9a01"}}}),
        "import.finished" => json!({"import": {"list_id": "0193a5b4-9d40-7b88-9f44-5c6d7e8f9a01", "created": 1200, "updated": 35, "skipped": 4, "errors": 2}}),
        _ => json!({}),
    }
}

//! Starter templates: well-made emails for the common jobs, built only from blocks and filled with
//! the account's brand (color, name, website and social links; the logo is added above every email
//! automatically). Flow emails use the triggering event's data and come with a sample event so the
//! gallery shows them filled in.

use chrono::{Duration, Utc};
use serde_json::{json, Value};

use crate::models::Account;

pub struct Brand {
    pub color: String,
    pub name: String,
    pub website: String,
    pub assets: String,
}

impl Brand {
    pub fn of(account: &Account, public_url: &str) -> Self {
        let website = account.website.as_deref().map(str::trim).filter(|w| !w.is_empty()).map(|w| if w.starts_with("http") { w.to_string() } else { format!("https://{w}") });
        // Brand text often comes from the customer's website; it must not open a template tag.
        let inert = |s: &str| s.replace(['{', '}'], "");
        Brand {
            color: crate::render::safe_color(&account.brand_color, "#1f6f5c"),
            name: inert(&account.name),
            website: inert(&website.unwrap_or_else(|| "https://example.com".into())),
            assets: format!("{public_url}/e/assets"),
        }
    }

    fn hero(&self) -> String {
        format!("{}/placeholder-hero.png", self.assets)
    }

    fn square(&self) -> String {
        format!("{}/placeholder-square.png", self.assets)
    }

    fn page(&self, path: &str) -> String {
        format!("{}/{}", self.website.trim_end_matches('/'), path.trim_start_matches('/'))
    }
}

pub struct Starter {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// "campaign" or "flow".
    pub kind: &'static str,
    /// The metric a flow using it is usually triggered by.
    pub trigger: Option<&'static str>,
}

pub const STARTERS: &[Starter] = &[
    Starter { key: "welcome", name: "Welcome", description: "Greet new subscribers and point them to your best stuff.", kind: "flow", trigger: None },
    Starter { key: "newsletter", name: "Newsletter", description: "A monthly roundup with stories, a quote and a call to action.", kind: "campaign", trigger: None },
    Starter { key: "product_launch", name: "Product launch", description: "Announce something new with a hero image, video and features.", kind: "campaign", trigger: None },
    Starter { key: "sale", name: "Sale", description: "A sale with a live countdown timer and featured products.", kind: "campaign", trigger: None },
    Starter { key: "abandoned_cart", name: "Abandoned cart", description: "Remind shoppers what's in their cart and link back to checkout.", kind: "flow", trigger: Some("Started Checkout") },
    Starter { key: "order_confirmation", name: "Order confirmation", description: "A receipt that lists each item from the order.", kind: "flow", trigger: Some("Placed Order") },
    Starter { key: "shipping", name: "Shipping update", description: "Tell customers their order shipped, with a tracking link.", kind: "flow", trigger: Some("Fulfilled Order") },
    Starter { key: "review_request", name: "Review request", description: "Ask for a review a week or two after delivery.", kind: "flow", trigger: Some("Fulfilled Order") },
    Starter { key: "win_back", name: "Win-back", description: "Bring back customers who haven't bought in a while.", kind: "flow", trigger: None },
    Starter { key: "birthday", name: "Birthday", description: "A birthday note with a gift.", kind: "flow", trigger: None },
    Starter { key: "back_in_stock", name: "Back in stock", description: "Let people know the item they wanted is available again.", kind: "flow", trigger: Some("Back in Stock") },
    Starter { key: "personal_note", name: "Personal note", description: "A plain, text-only note that reads like it came from a person.", kind: "campaign", trigger: None },
    Starter { key: "booking_confirmation", name: "Event or booking confirmation", description: "Confirm a booking or event with the details in a table.", kind: "flow", trigger: Some("Booked") },
    Starter { key: "pre_arrival", name: "Pre-arrival info", description: "Arrival details for a stay. Send it only from a flow that runs for fully paid bookings.", kind: "flow", trigger: Some("Booked") },
];

pub fn find(key: &str) -> Option<&'static Starter> {
    STARTERS.iter().find(|s| s.key == key)
}

fn heading(text: &str, level: u8, align: &str) -> Value {
    json!({"type": "heading", "text": text, "level": level, "align": align})
}

fn text(html: &str, align: &str) -> Value {
    json!({"type": "text", "html": html, "align": align})
}

fn button(text: &str, href: &str) -> Value {
    json!({"type": "button", "text": text, "href": href, "align": "center"})
}

fn image(src: &str, alt: &str, href: &str) -> Value {
    json!({"type": "image", "src": src, "alt": alt, "href": href})
}

fn spacer(h: u32) -> Value {
    json!({"type": "spacer", "height": h})
}

fn footer(b: &Brand) -> Value {
    json!({"type": "footer", "text": "Questions? Just reply to this email.", "links": [{"text": "Visit our website", "href": b.website}], "style": "light"})
}

fn nav(b: &Brand) -> Value {
    json!({"type": "nav", "links": [{"text": "Shop", "href": b.page("shop")}, {"text": "New", "href": b.page("new")}, {"text": "About", "href": b.page("about")}], "background": b.color, "color": "#ffffff"})
}

fn items_table() -> Value {
    json!({"type": "table",
        "columns": [{"label": "Item"}, {"label": "Qty", "align": "center"}, {"label": "Price", "align": "right"}],
        "loop": {"source": "event.properties.items", "cells": ["{{ item.name }}", "{{ item.quantity|default(1) }}", "{{ item.price }}"]}})
}

/// The starter's email content for this brand.
pub fn content(key: &str, b: &Brand) -> Option<Value> {
    let c = &b.color;
    let name = &b.name;
    let (subject, preheader, blocks): (String, &str, Vec<Value>) = match key {
        "welcome" => (
            format!("Welcome to {name}"),
            "We're glad you're here. Here's where to start.",
            vec![
                image(&b.hero(), "Welcome", &b.website),
                heading(&format!("Welcome to {name}"), 1, "center"),
                text("<p>Hi {{ first_name|default('there') }}, thanks for joining us. Here's what you can expect: new arrivals first, member-only offers, and the occasional story from our team.</p>", "center"),
                button("Start exploring", &b.website),
                spacer(8),
                json!({"type": "columns", "columns": [
                    [heading("Made to last", 3, "center"), text("<p>Things we'd use ourselves, built to stick around.</p>", "center")],
                    [heading("Free returns", 3, "center"), text("<p>Not right? Send it back within 30 days.</p>", "center")],
                    [heading("Real people", 3, "center"), text("<p>Reply to any email and a person answers.</p>", "center")]
                ]}),
                footer(b),
            ],
        ),
        "newsletter" => (
            format!("What's new at {name} this month"),
            "Stories, new arrivals and one thing we can't stop talking about.",
            vec![
                nav(b),
                heading(&format!("This month at {name}"), 1, "left"),
                text("<p>Hi {{ first_name|default('there') }}, here's what we've been working on, and a few things we think you'll like.</p>", "left"),
                json!({"type": "image_text", "src": b.square(), "alt": "Story photo", "heading": "The story behind our newest piece", "html": "<p>Two sentences about where it came from and why it matters.</p>", "button_text": "Read more", "button_href": b.page("blog"), "image_position": "left"}),
                json!({"type": "image_text", "src": b.square(), "alt": "Behind the scenes", "heading": "Behind the scenes", "html": "<p>A quick look at how things get made, and who makes them.</p>", "button_text": "Take a look", "button_href": b.page("about"), "image_position": "right"}),
                json!({"type": "divider"}),
                json!({"type": "quote", "text": "The best thing I bought all year. I've already ordered a second one.", "author": "Jordan P.", "role": "Customer since 2023", "style": "card"}),
                button("See everything new", &b.page("new")),
                footer(b),
            ],
        ),
        "product_launch" => (
            "It's here".to_string(),
            "Meet the newest addition to the lineup.",
            vec![
                image(&b.hero(), "The new product", &b.page("new")),
                heading("Meet the new one", 1, "center"),
                text("<p>Say what it is, who it's for and why it's better, in two short sentences.</p>", "center"),
                button("Shop now", &b.page("new")),
                json!({"type": "video", "url": b.page("new"), "alt": "Watch it in action", "caption": "See it in action (1 min)"}),
                json!({"type": "columns", "columns": [
                    [image(&b.square(), "Feature one", ""), heading("Feature one", 3, "left"), text("<p>What it does for them.</p>", "left")],
                    [image(&b.square(), "Feature two", ""), heading("Feature two", 3, "left"), text("<p>What it does for them.</p>", "left")]
                ]}),
                // The newest products in the catalog (shows nothing until the catalog has products).
                json!({"type": "products", "source": "newest", "count": 2, "columns": 2}),
                footer(b),
            ],
        ),
        "sale" => (
            "Our biggest sale of the year starts now".to_string(),
            "Up to 30% off, for a few days only.",
            vec![
                nav(b),
                heading("Up to 30% off everything", 1, "center"),
                text("<p>Our biggest sale of the year is on. It ends when the clock hits zero.</p>", "center"),
                json!({"type": "countdown", "ends_at": (Utc::now() + Duration::days(7)).format("%Y-%m-%dT%H:00:00Z").to_string(), "background": "#111827", "color": "#ffffff", "label_color": "#9ca3af", "ended_text": "This sale has ended"}),
                button("Shop the sale", &b.page("sale")),
                json!({"type": "columns", "columns": [
                    [{"type": "product", "title": "Best seller", "image": b.square(), "price": "$48", "href": b.page("sale"), "button_text": "Shop"}],
                    [{"type": "product", "title": "Staff pick", "image": b.square(), "price": "$36", "href": b.page("sale"), "button_text": "Shop"}]
                ]}),
                footer(b),
            ],
        ),
        "abandoned_cart" => (
            "You left something in your cart".to_string(),
            "We saved it for you.",
            vec![
                heading("Still thinking it over?", 1, "center"),
                text("<p>Hi {{ first_name|default('there') }}, you left these in your cart. They're still there when you're ready.</p>", "center"),
                items_table(),
                button("Finish checking out", &format!("{{{{ event.properties.checkout_url|default('{}') }}}}", b.page("cart"))),
                text("<p>Questions about sizing or shipping? Just reply.</p>", "center"),
                footer(b),
            ],
        ),
        "order_confirmation" => (
            "Thanks for your order".to_string(),
            "Here's your receipt.",
            vec![
                heading("Thanks for your order!", 1, "left"),
                text("<p>Hi {{ first_name|default('there') }}, we got your order <strong>#{{ event.properties.order_id }}</strong> and we're getting it ready. We'll email you when it ships.</p>", "left"),
                json!({"type": "table",
                    "columns": [{"label": "Item"}, {"label": "Qty", "align": "center"}, {"label": "Price", "align": "right"}],
                    "loop": {"source": "event.properties.items", "cells": ["{{ item.name }}", "{{ item.quantity|default(1) }}", "{{ item.price }}"]},
                    "footer": [["Total", "", "{{ event.value }}"]]}),
                button("View your order", &b.page("account")),
                footer(b),
            ],
        ),
        "shipping" => (
            "Your order is on its way".to_string(),
            "Track your package.",
            vec![
                heading("It's on the way", 1, "center"),
                text("<p>Hi {{ first_name|default('there') }}, your order <strong>#{{ event.properties.order_id }}</strong> has shipped.</p>", "center"),
                json!({"type": "table", "columns": [{"label": ""}, {"label": "", "align": "right"}],
                    "rows": [["Carrier", "{{ event.properties.carrier|default('See tracking') }}"], ["Tracking number", "{{ event.properties.tracking_number|default('See tracking') }}"]]}),
                button("Track your package", &format!("{{{{ event.properties.tracking_url|default('{}') }}}}", b.page("account"))),
                footer(b),
            ],
        ),
        "review_request" => (
            "How are you liking it?".to_string(),
            "Your review helps other people decide.",
            vec![
                heading("How did we do?", 1, "center"),
                text("<p>Hi {{ first_name|default('there') }}, you've had your order for a little while now. Would you take a minute to tell other people what you think?</p>", "center"),
                button("Write a review", &b.page("reviews")),
                json!({"type": "quote", "text": "Reviews like yours are how small shops like ours get found. Thank you.", "author": format!("The {name} team"), "style": "plain", "color": c}),
                footer(b),
            ],
        ),
        "win_back" => (
            "We miss you, {{ first_name|default('friend') }}".to_string(),
            "Here's 15% off to come back.",
            vec![
                image(&b.hero(), "We miss you", &b.website),
                heading("It's been a while", 1, "center"),
                text("<p>A lot has changed since your last visit. Come see what's new, and take 15% off your next order with code <strong>COMEBACK15</strong>.</p>", "center"),
                button("Take 15% off", &b.website),
                footer(b),
            ],
        ),
        "birthday" => (
            "Happy birthday, {{ first_name|default('friend') }}!".to_string(),
            "A little gift from us.",
            vec![
                image(&b.hero(), "Happy birthday", &b.website),
                heading("Happy birthday, {{ first_name|default('friend') }}!", 1, "center"),
                text("<p>To celebrate, here's 20% off anything this week with code <strong>BIRTHDAY20</strong>.</p>", "center"),
                button("Treat yourself", &b.website),
                footer(b),
            ],
        ),
        "back_in_stock" => (
            "It's back in stock".to_string(),
            "The item you wanted is available again.",
            vec![
                heading("Good news: it's back", 1, "center"),
                text("<p>Hi {{ first_name|default('there') }}, the item you asked about is available again. It sold out fast last time, so don't wait too long.</p>", "center"),
                // The product from the triggering event, with its photo and price from the catalog.
                json!({"type": "products", "source": "event_items", "count": 1, "columns": 1, "button_text": "Get it now"}),
                button("Shop now", &format!("{{{{ event.properties.url|default('{}') }}}}", b.website)),
                footer(b),
            ],
        ),
        "personal_note" => (
            "A quick note".to_string(),
            "",
            vec![
                text("<p>Hi {{ first_name|default('there') }},</p><p>I wanted to write to you myself. Say the one thing you want them to know, the way you'd say it to a friend.</p><p>If you have a minute, just hit reply. I read every answer.</p>", "left"),
                text(&format!("<p>Thanks,<br>The team at {}</p>", crate::features::content::blocks::esc(name)), "left"),
            ],
        ),
        "booking_confirmation" => (
            "You're booked".to_string(),
            "Here are the details.",
            vec![
                heading("You're booked!", 1, "left"),
                text("<p>Hi {{ first_name|default('there') }}, thanks for booking with us. Here are your details.</p>", "left"),
                json!({"type": "table", "columns": [{"label": ""}, {"label": "", "align": "right"}],
                    "rows": [["What", "{{ event.properties.name|default('Your booking') }}"], ["Starts", "{{ event.properties.start_date }}"], ["Ends", "{{ event.properties.end_date }}"], ["Confirmation", "{{ event.properties.confirmation_code }}"]]}),
                button("View your booking", &format!("{{{{ event.properties.url|default('{}') }}}}", b.website)),
                footer(b),
            ],
        ),
        "pre_arrival" => (
            "Your stay is almost here".to_string(),
            "Everything you need for arrival day.",
            vec![
                image(&b.hero(), "Your stay", ""),
                heading("See you soon, {{ first_name|default('there') }}", 1, "left"),
                text("<p>Your stay starts soon. Here's what you need for arrival day. Save this email so it's handy when you get there.</p>", "left"),
                json!({"type": "table", "columns": [{"label": ""}, {"label": "", "align": "right"}],
                    "rows": [["Check-in", "{{ event.properties.check_in|default('From 4:00 PM') }}"], ["Check-out", "{{ event.properties.check_out|default('By 10:00 AM') }}"], ["Address", "{{ event.properties.address }}"], ["Parking", "{{ event.properties.parking|default('Ask us') }}"]]}),
                button("Get directions", "{{ event.properties.directions_url|default('https://maps.google.com') }}"),
                text("<p>Anything you need before you arrive? Just reply to this email.</p>", "left"),
                footer(b),
            ],
        ),
        _ => return None,
    };
    let mut content = json!({"subject": subject, "blocks": blocks, "theme": {"brand_color": c}});
    if !preheader.is_empty() {
        content["preheader"] = json!(preheader);
    }
    Some(content)
}

/// Sample event data so flow emails show filled in when previewed.
pub fn sample_event(key: &str) -> Option<Value> {
    let items = json!([{"name": "Everyday tote", "quantity": 1, "price": "$48.00"}, {"name": "Canvas cap", "quantity": 2, "price": "$36.00"}]);
    match key {
        "abandoned_cart" => Some(json!({"value": 84.0, "properties": {"items": items}})),
        "order_confirmation" => Some(json!({"value": "$84.00", "properties": {"order_id": "1042", "items": items}})),
        "shipping" => Some(json!({"properties": {"order_id": "1042", "carrier": "UPS", "tracking_number": "1Z999AA10123456784"}})),
        "back_in_stock" => Some(json!({"properties": {"product_name": "Everyday tote", "product_id": "TOTE-1"}})),
        "booking_confirmation" => Some(json!({"properties": {"name": "Sunset kayak tour", "start_date": "Sat, Jun 6 at 6:00 PM", "end_date": "Sat, Jun 6 at 8:00 PM", "confirmation_code": "K7Q2P"}})),
        "pre_arrival" => Some(json!({"properties": {"address": "123 Ocean View Rd"}})),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_starter_has_content_with_the_brand_color() {
        let b = Brand { color: "#aa3366".into(), name: "Acme".into(), website: "https://acme.test".into(), assets: "https://pl.test/e/assets".into() };
        assert!((12..=15).contains(&STARTERS.len()));
        for s in STARTERS {
            let c = content(s.key, &b).unwrap_or_else(|| panic!("{}", s.key));
            assert_eq!(c["theme"]["brand_color"], "#aa3366", "{}", s.key);
            assert!(!c["subject"].as_str().unwrap().is_empty(), "{}", s.key);
            assert!(!c["blocks"].as_array().unwrap().is_empty(), "{}", s.key);
            assert!(!c.to_string().contains("\"html\":\"<html"), "{}", s.key);
        }
        assert!(content("nope", &b).is_none());
    }
}

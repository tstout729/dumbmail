//! Product catalog: how many queries filling product blocks costs. In its own test binary because
//! it counts queries through a tracing layer, which other tests running in the same process at the
//! same time would disturb.

#[macro_use]
mod common;

use serde_json::{json, Value};
use std::sync::Arc;


/// Finding 9: lists that are the same for everyone are read once per send, and an event block's
/// queries don't grow with the number of items.
#[tokio::test]
async fn catalog_lookups_dont_repeat_per_recipient() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::layer::SubscriberExt;
    struct Count(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Count {
        fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }
        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::TRACE)
        }
        fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            if e.metadata().target().starts_with("sqlx::query") {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    let mut h = harness!();
    let me = h.signup().await;
    let account_id: uuid::Uuid = me["account"]["id"].as_str().unwrap().parse().unwrap();
    let many: Vec<Value> = (0..12).map(|i| json!({"external_id": format!("P{i}"), "title": format!("Thing {i}"), "price": 10 + i})).collect();
    h.ok("POST", "/api/catalog/products", Some(json!({"products": many}))).await;
    for i in 0..3 {
        h.ok("POST", "/api/profiles", Some(json!({"email": format!("r{i}@x.test")}))).await;
    }
    let account = dumbmail::campaigns::load_account(&h.state.db, account_id).await.unwrap();
    let q = format!("SELECT {} FROM profiles p ORDER BY email", dumbmail::models::PROFILE_COLS);
    let people: Vec<dumbmail::models::Profile> = sqlx::query_as(&q).fetch_all(&h.state.db).await.unwrap();
    let n = Arc::new(AtomicUsize::new(0));
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(Count(n.clone())));
    let run = |content: Value, event: Option<Value>, p: usize| {
        let (db, account, person, n) = (h.state.db.clone(), account.clone(), people[p].clone(), n.clone());
        async move {
            let before = n.load(Ordering::SeqCst);
            let mut ctx = json!({"person": {}, "organization": {}});
            dumbmail::features::catalog::personalize(&db, &account, Some(&person), event.as_ref(), &content, &mut ctx, false).await.unwrap();
            n.load(Ordering::SeqCst) - before
        }
    };
    let shared = json!({"subject": "{{ catalog.products['P1'].title }} {{ catalog.best_sellers[0].title }}", "blocks": [
        {"type": "products", "source": "handpicked", "product_ids": ["P2", "P3"]}, {"type": "products", "source": "newest"}, {"type": "products", "source": "on_sale", "fallback": "best_sellers"}]});
    let first = run(shared.clone(), None, 0).await;
    assert!(first > 0, "queries are counted");
    assert_eq!(run(shared.clone(), None, 1).await, 0, "the next recipient reads nothing again");
    assert_eq!(run(shared, None, 2).await, 0);
    let items = |k: usize| json!({"metric": "Started Checkout", "properties": {"items": (0..k).map(|i| json!({"name": format!("Thing {i}"), "product_id": format!("P{i}"), "sku": format!("S{i}")})).collect::<Vec<_>>()}});
    let block = json!({"subject": "s", "blocks": [{"type": "products", "source": "event_items", "count": 1}]});
    let one = run(block.clone(), Some(items(1)), 0).await;
    let twelve = run(block, Some(items(12)), 0).await;
    assert!(one <= 5 && twelve == one, "one item: {one} queries, twelve items: {twelve}");
    let recs = run(json!({"subject": "s", "blocks": [{"type": "products", "source": "recommended"}]}), None, 0).await;
    assert!(recs <= 12, "picks for a person: {recs} queries");
    drop(_guard);
    h.cleanup().await;
}

pub mod account;
pub mod analytics;
pub mod audience;
pub mod messaging;

use axum::extract::{FromRequest, Request};
use axum::Router;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::state::AppState;

/// JSON body extractor that reports problems in the API's error format.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = AppError;
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(ApiJson(v)),
            Err(e) => Err(AppError::BadRequest(e.body_text())),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Page {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl Page {
    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(50).clamp(1, 500)
    }
    pub fn offset(&self) -> i64 {
        self.offset.unwrap_or(0).max(0)
    }
}

pub fn page<T: serde::Serialize>(data: Vec<T>, total: i64) -> axum::Json<Value> {
    axum::Json(json!({ "data": data, "total": total }))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(account::routes())
        .merge(audience::routes())
        .merge(messaging::routes())
        .merge(analytics::routes())
}

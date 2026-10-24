//! The HTTP surface: the endpoints of SRS §3.4 plus the landing page at `/`.
//! Any other path or method gets 404.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, get_service};
use axum::{Json, Router};
use serde::Serialize;
use sqlx::PgPool;
use tower_http::services::ServeFile;

use crate::{catalog, dispatcher, persistence, webhook};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub webhook: webhook::Verifier,
    /// Wakes the dispatcher once the webhook has stored events
    pub wakeup: dispatcher::Wakeup,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get_service(ServeFile::new("static/index.html")))
        .route("/health", get(health))
        .merge(webhook::router())
        .merge(catalog::router())
        .fallback(not_found)
        // Only covers routes added before it, so it stays last
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    database: &'static str,
}

/// Liveness and readiness (NFR-MNT-04). Public, so it reveals nothing beyond up or down.
async fn health(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    let version = env!("CARGO_PKG_VERSION");
    if persistence::ping(&state.db).await {
        (
            StatusCode::OK,
            Json(Health {
                status: "ok",
                version,
                database: "up",
            }),
        )
    } else {
        let health = Health {
            status: "unavailable",
            version,
            database: "down",
        };
        (StatusCode::SERVICE_UNAVAILABLE, Json(health))
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, header};
    use axum::response::Response;
    use tower::ServiceExt;

    use super::*;
    use crate::config::Secret;
    use crate::persistence::testing;

    fn app() -> Router {
        router(AppState {
            db: testing::unreachable(),
            webhook: webhook::Verifier::new(&Secret::new("channel-secret")),
            wakeup: dispatcher::Wakeup::default(),
        })
    }

    async fn send(method: Method, uri: &str) -> Response {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        app().oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn landing_page_is_served_at_root() {
        let response = send(Method::GET, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
    }

    #[tokio::test]
    async fn anything_else_is_not_found() {
        for (method, uri) in [
            (Method::GET, "/index.html"),
            (Method::POST, "/"),
            (Method::GET, "/foo"),
            (Method::GET, "/health/"),
            (Method::POST, "/health"),
            (Method::GET, "/webhook/line"),
            (Method::POST, "/img/p/abc"),
            (Method::GET, "/img/p"),
        ] {
            let status = send(method.clone(), uri).await.status();
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }

    /// A malformed ID or token is refused without asking the database.
    #[tokio::test]
    async fn image_links_need_a_well_formed_id() {
        for uri in [
            "/img/p/abc",
            "/img/s/abc",
            "/img/p/0123456789abcdef0123456789abcdeg",
            "/img/s/..%2F..%2Fhealth",
        ] {
            let status = send(Method::GET, uri).await.status();
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn image_links_are_unavailable_without_the_database() {
        for uri in [
            "/img/p/0123456789abcdef0123456789abcdef",
            "/img/s/01234567-89ab-cdef-0123-456789abcdef",
        ] {
            let status = send(Method::GET, uri).await.status();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
    }

    #[tokio::test]
    async fn health_is_unavailable_without_the_database() {
        let response = send(Method::GET, "/health").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "status": "unavailable",
                "version": env!("CARGO_PKG_VERSION"),
                "database": "down",
            })
        );
    }
}

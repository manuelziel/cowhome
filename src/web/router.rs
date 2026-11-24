//! `axum` REST API v1 router construction, body-size guard (`1 MiB`), & `404` fallback.
//!
//! Wires all 15 `/api/v1/*` endpoints declared in [`crate::core::constants::route_paths`]
//! to their domain handlers and layered authentication/security middlewares.

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::response::Response;
use axum::routing::{get, post};

use super::handlers::{
    automation::{
        get_automation_all, get_automation_feature, put_automation_batch, put_automation_feature,
    },
    device_action::post_device_action,
    groups::{create_group, delete_group, get_group_item, list_groups, update_group},
    state_weather::{get_docs_ui, get_history, get_openapi_spec, get_state, get_weather},
    system::{
        get_system_calls, get_system_diagnostics, get_system_energy, get_system_hosts,
        get_system_status, get_system_storage, put_system_energy,
    },
};
use super::middleware::{auth_middleware, docs_guard_middleware, security_headers_and_logging};
use super::response::ApiError;
use super::state::AppState;
use crate::core::constants::{
    route_paths::{
        AUTOMATION, AUTOMATION_FEATURE, DEVICE_ACTION, DOCS_UI, GROUP_ITEM, GROUPS, HISTORY,
        OPENAPI_SPEC, STATE, SYSTEM_AI_DIAGNOSTICS, SYSTEM_CALLS, SYSTEM_DIAGNOSTICS,
        SYSTEM_ENERGY, SYSTEM_HOSTS, SYSTEM_SETTINGS, SYSTEM_STATUS, SYSTEM_STORAGE, WEATHER,
    },
    service_defaults::MAX_HTTP_BODY_BYTES,
};
use crate::core::error::AppError;

// =============================================================================
/// Builds the complete `axum` [`Router`] wired to `state`.
pub fn build_router(state: Arc<AppState>) -> Router {
    let docs_trailing = format!("{DOCS_UI}/");
    let docs_routes = Router::new()
        .route(DOCS_UI, get(get_docs_ui))
        .route(&docs_trailing, get(get_docs_ui))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            docs_guard_middleware,
        ));

    let protected_routes = Router::new()
        .route(STATE, get(get_state))
        .route(DEVICE_ACTION, post(post_device_action))
        .route(GROUPS, get(list_groups).post(create_group))
        .route(
            GROUP_ITEM,
            get(get_group_item).put(update_group).delete(delete_group),
        )
        .route(HISTORY, get(get_history))
        .route(WEATHER, get(get_weather))
        .route(
            AUTOMATION,
            get(get_automation_all).put(put_automation_batch),
        )
        .route(
            AUTOMATION_FEATURE,
            get(get_automation_feature).put(put_automation_feature),
        )
        .route(SYSTEM_STATUS, get(get_system_status))
        .route(SYSTEM_STORAGE, get(get_system_storage))
        .route(SYSTEM_HOSTS, get(get_system_hosts))
        .route(SYSTEM_CALLS, get(get_system_calls))
        .route(SYSTEM_ENERGY, get(get_system_energy).put(put_system_energy))
        .route(
            SYSTEM_SETTINGS,
            get(get_system_energy).put(put_system_energy),
        )
        .route(SYSTEM_DIAGNOSTICS, get(get_system_diagnostics))
        .route(SYSTEM_AI_DIAGNOSTICS, get(get_system_diagnostics))
        .route(OPENAPI_SPEC, get(get_openapi_spec))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth_middleware,
        ));

    Router::new()
        .merge(docs_routes)
        .merge(protected_routes)
        .fallback(not_found_handler)
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            security_headers_and_logging,
        ))
        .with_state(state)
}

// =============================================================================
/// Standardized JSON `404 Not Found` fallback handler.
async fn not_found_handler() -> Response {
    use axum::response::IntoResponse;
    ApiError::new(AppError::NotFound(String::from("Endpoint not found"))).into_response()
}

//! Read-only `::hot::store` browser handlers.
//!
//! These render the same store backend the runtime writes to (Postgres in cloud,
//! SQLite locally) scoped to the current session's `(org_id, env_id)`. Admin users
//! can additionally delete individual entries; create/edit are intentionally not
//! exposed.

use crate::auth::{AppState, Session};
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

const ENTRIES_PER_PAGE: i64 = 50;

fn no_store_response(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn no_store_text_response(status: StatusCode, body: &'static str) -> Response {
    no_store_response(
        Response::builder()
            .status(status)
            .body(axum::body::Body::from(body))
            .unwrap(),
    )
}

/// Build a backend store handle for the current request, scoped to the session's org/env.
async fn build_store(
    state: &AppState,
    session: &Session,
) -> Result<Box<dyn hot::store::Store>, String> {
    let org_id = session
        .current_org_id()
        .ok_or_else(|| "No current organization".to_string())?;
    let env_id = session.current_env_id();
    hot::store::store_from_config_with_db(&state.conf, Some(state.db.clone()), Some(org_id), env_id)
        .await
}

/// `GET /stores` — list every named store map in the current env.
pub async fn stores_list_handler(
    State(state): State<AppState>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Response {
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Stores".to_string()));

    let search_query = params
        .get("search")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let (stores, error_message, storage_type) = match build_store(&state, &session).await {
        Ok(store) => {
            let storage_type = store.storage_type().to_string();
            match store.list_maps().await {
                Ok(infos) => {
                    let needle = search_query.to_lowercase();
                    let mut display: Vec<templates::StoreMapDisplay> = infos
                        .iter()
                        .filter(|m| needle.is_empty() || m.name.to_lowercase().contains(&needle))
                        .map(templates::StoreMapDisplay::from)
                        .collect();
                    display.sort_by(|a, b| a.name.cmp(&b.name));
                    (display, None, storage_type)
                }
                Err(e) => {
                    tracing::error!("list_maps failed: {e}");
                    (Vec::new(), Some(e), storage_type)
                }
            }
        }
        Err(e) => {
            tracing::warn!("Store backend unavailable for stores_list_handler: {e}");
            (Vec::new(), Some(e), "unavailable".to_string())
        }
    };

    let total_stores = stores.len();

    let template = templates::StoresList {
        title: "Stores",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "stores",
            &session,
            breadcrumbs,
        ),
        stores,
        total_stores,
        search_query,
        error_message,
        storage_type,
    };

    Html(template.render().unwrap()).into_response()
}

/// `GET /stores/{store_name}` — store summary + paginated entries.
pub async fn store_detail_handler(
    State(state): State<AppState>,
    Path(store_name_url): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    headers: HeaderMap,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Response {
    let is_htmx_request = crate::handlers::is_htmx_request(&headers);
    let store_name = match urlencoding::decode(&store_name_url) {
        Ok(s) => s.into_owned(),
        Err(_) => return Redirect::to("/stores").into_response(),
    };

    let store = match build_store(&state, &session).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Store backend unavailable for store_detail_handler: {e}");
            return Redirect::to("/stores").into_response();
        }
    };

    // Find this store in the current scope's `list_maps` so we can render summary metadata.
    let info = match store.list_maps().await {
        Ok(maps) => maps.into_iter().find(|m| m.name == store_name),
        Err(e) => {
            tracing::error!("list_maps failed for store {}: {e}", store_name);
            None
        }
    };
    let info = match info {
        Some(i) => i,
        None => return Redirect::to("/stores").into_response(),
    };

    let store_display = templates::StoreMapDisplay::from(&info);
    let search_query = params
        .get("search")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let is_searching = !search_query.is_empty();

    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1);
    let offset = (current_page_num - 1) * ENTRIES_PER_PAGE;

    let query_embedding = if is_searching && info.embedding_model.is_some() {
        match hot::store::embedding::embedding_provider_from_config(&state.conf) {
            Some(provider) => match provider.embed(&search_query).await {
                Ok(embedding) => Some(embedding),
                Err(e) => {
                    tracing::warn!(
                        "Store search embedding failed for '{}'; falling back to keyword search: {e}",
                        store_name
                    );
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };

    let (entries, total_entries, error_message) = if is_searching {
        let search_mode = if query_embedding.is_some() {
            hot::store::SearchMode::Hybrid
        } else {
            hot::store::SearchMode::Keyword
        };

        match store
            .search_info(
                &store_name,
                Some(&search_query),
                query_embedding,
                hot::store::SearchOptions {
                    limit: None,
                    min_score: None,
                    mode: search_mode,
                },
                hot::store::ListOptions {
                    limit: Some(ENTRIES_PER_PAGE as usize),
                    offset: Some(offset as usize),
                },
            )
            .await
        {
            Ok(page) => (
                page.entries
                    .iter()
                    .map(templates::StoreEntryDisplay::from_info)
                    .collect(),
                page.total_entries as i64,
                None,
            ),
            Err(e) => {
                tracing::error!("store search failed for {}: {e}", store_name);
                (Vec::new(), 0, Some(e))
            }
        }
    } else {
        match store
            .list_info(
                &store_name,
                hot::store::ListOptions {
                    limit: Some(ENTRIES_PER_PAGE as usize),
                    offset: Some(offset as usize),
                },
            )
            .await
        {
            Ok(rows) => (
                rows.iter()
                    .map(templates::StoreEntryDisplay::from_info)
                    .collect(),
                info.entry_count,
                None,
            ),
            Err(e) => {
                tracing::error!("store list failed for {}: {e}", store_name);
                (Vec::new(), info.entry_count, Some(e))
            }
        }
    };

    let total_pages = if total_entries > 0 {
        (total_entries + ENTRIES_PER_PAGE - 1) / ENTRIES_PER_PAGE
    } else {
        1
    };

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Stores".to_string(),
        "/stores".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(store_name.clone()));

    let has_prev_page = current_page_num > 1;
    let has_next_page = current_page_num < total_pages;
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let deleted_flash = params.get("deleted").map(|s| s == "1").unwrap_or(false);
    let pagination_search_suffix = if is_searching {
        format!("&search={}", urlencoding::encode(&search_query))
    } else {
        String::new()
    };

    if is_htmx_request {
        let partial = templates::StoreEntriesTable {
            page_context: templates::PrivatePageContext::with_breadcrumbs(
                "stores",
                &session,
                breadcrumbs,
            ),
            store: store_display,
            entries,
            current_page_num,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_entries,
            is_admin: session.is_current_org_admin,
            is_searching,
            pagination_search_suffix,
        };
        return Html(partial.render().unwrap()).into_response();
    }

    let template = templates::StoreDetail {
        title: "Store",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "stores",
            &session,
            breadcrumbs,
        ),
        store: store_display,
        entries,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_entries,
        is_admin: session.is_current_org_admin,
        deleted_flash,
        error_message,
        search_query,
        is_searching,
        pagination_search_suffix,
    };

    Html(template.render().unwrap()).into_response()
}

/// `GET /stores/{store_name}/entries/{key_encoded}` — full key/value detail.
pub async fn entry_detail_handler(
    State(state): State<AppState>,
    Path((store_name_url, key_encoded)): Path<(String, String)>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Response {
    let store_name = match urlencoding::decode(&store_name_url) {
        Ok(s) => s.into_owned(),
        Err(_) => return Redirect::to("/stores").into_response(),
    };

    let key = match templates::decode_entry_key(&key_encoded) {
        Ok(k) => k,
        Err(_) => {
            return Redirect::to(&format!("/stores/{}", store_name_url)).into_response();
        }
    };

    let store = match build_store(&state, &session).await {
        Ok(s) => s,
        Err(_) => return Redirect::to("/stores").into_response(),
    };

    let info = match store.list_maps().await {
        Ok(maps) => maps.into_iter().find(|m| m.name == store_name),
        Err(_) => None,
    };
    let info = match info {
        Some(i) => i,
        None => return Redirect::to("/stores").into_response(),
    };
    let store_display = templates::StoreMapDisplay::from(&info);

    let entry = match store.get_info(&store_name, &key).await {
        Ok(Some(e)) => templates::StoreEntryDisplay::from_info(&e),
        Ok(None) => {
            return Redirect::to(&format!("/stores/{}", store_name_url)).into_response();
        }
        Err(e) => {
            tracing::error!(
                "store get_info failed for {}/{}: {e}",
                store_name,
                key_encoded
            );
            return Redirect::to(&format!("/stores/{}", store_name_url)).into_response();
        }
    };

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Stores".to_string(),
        "/stores".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        store_name.clone(),
        format!("/stores/{}", store_name_url),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        templates::truncate_string(&entry.key_preview, 60),
    ));

    let template = templates::EntryDetail {
        title: "Store Entry",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "stores",
            &session,
            breadcrumbs,
        ),
        store: store_display,
        entry,
        is_admin: session.is_current_org_admin,
    };

    Html(template.render().unwrap()).into_response()
}

#[derive(Debug, Deserialize)]
pub struct RevealValueParams {
    #[serde(default)]
    pub view: String,
}

/// `GET /stores/{store_name}/entries/{key_encoded}/value` — admin-only value reveal.
pub async fn entry_value_handler(
    State(state): State<AppState>,
    Path((store_name_url, key_encoded)): Path<(String, String)>,
    Query(params): Query<RevealValueParams>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Response {
    if !session.is_current_org_admin {
        return no_store_text_response(
            StatusCode::FORBIDDEN,
            "Only organization admins can reveal store values.",
        );
    }

    let store_name = match urlencoding::decode(&store_name_url) {
        Ok(s) => s.into_owned(),
        Err(_) => {
            return no_store_text_response(StatusCode::BAD_REQUEST, "Invalid store name.");
        }
    };

    let key = match templates::decode_entry_key(&key_encoded) {
        Ok(k) => k,
        Err(_) => {
            return no_store_text_response(StatusCode::BAD_REQUEST, "Invalid entry key.");
        }
    };

    // The "hidden-cell" / "hidden-mobile-cell" views are static fragments that
    // do not require the actual value, so we can short-circuit before opening
    // a backend connection.
    let view_kind = ValueViewKind::parse(&params.view);
    if let ValueViewKind::HiddenCell { mobile } = view_kind {
        let container_id = container_id_for(mobile, &key_encoded);
        let view = if mobile { "mobile-cell" } else { "cell" }.to_string();
        let template = templates::StoreEntryValueHiddenCell {
            key_encoded: key_encoded.clone(),
            container_id,
            view,
        };
        return no_store_response(Html(template.render().unwrap()).into_response());
    }

    let store = match build_store(&state, &session).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Store backend unavailable for entry_value_handler: {e}");
            return no_store_text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Store backend unavailable.",
            );
        }
    };

    let entry = match store.get(&store_name, &key).await {
        Ok(Some(e)) => templates::StoreEntryDisplay::from(&e),
        Ok(None) => {
            return no_store_text_response(StatusCode::NOT_FOUND, "Store entry not found.");
        }
        Err(e) => {
            tracing::error!("store get failed for {}/{}: {e}", store_name, key_encoded);
            return no_store_text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Store entry lookup failed.",
            );
        }
    };

    match view_kind {
        ValueViewKind::Panel => {
            let template = templates::StoreEntryValuePanel { entry };
            no_store_response(Html(template.render().unwrap()).into_response())
        }
        ValueViewKind::Cell { mobile } => {
            let container_id = container_id_for(mobile, &entry.key_encoded);
            let view = if mobile { "mobile-cell" } else { "cell" }.to_string();
            let template = templates::StoreEntryValueCell {
                entry,
                container_id,
                view,
            };
            no_store_response(Html(template.render().unwrap()).into_response())
        }
        ValueViewKind::HiddenCell { .. } => unreachable!("handled above"),
    }
}

#[derive(Debug, Clone, Copy)]
enum ValueViewKind {
    Cell { mobile: bool },
    HiddenCell { mobile: bool },
    Panel,
}

impl ValueViewKind {
    fn parse(view: &str) -> Self {
        match view {
            "panel" => ValueViewKind::Panel,
            "mobile-cell" => ValueViewKind::Cell { mobile: true },
            "hidden-cell" => ValueViewKind::HiddenCell { mobile: false },
            "hidden-mobile-cell" => ValueViewKind::HiddenCell { mobile: true },
            _ => ValueViewKind::Cell { mobile: false },
        }
    }
}

fn container_id_for(mobile: bool, key_encoded: &str) -> String {
    if mobile {
        format!("m-entry-value-{}", key_encoded)
    } else {
        format!("entry-value-{}", key_encoded)
    }
}

#[derive(Debug, Deserialize)]
pub struct DeleteEntryForm {
    pub key_encoded: String,
    /// Optional redirect target after delete: "list" (default) or "store".
    #[serde(default)]
    pub origin: String,
}

/// `POST /stores/{store_name}/entries/delete` — admin-only delete for a single key.
pub async fn entry_delete_handler(
    State(state): State<AppState>,
    Path(store_name_url): Path<String>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<DeleteEntryForm>,
) -> Response {
    if !session.is_current_org_admin {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(axum::body::Body::from(
                "Only organization admins can delete store entries.",
            ))
            .unwrap();
    }

    let store_name = match urlencoding::decode(&store_name_url) {
        Ok(s) => s.into_owned(),
        Err(_) => return Redirect::to("/stores").into_response(),
    };

    let key = match templates::decode_entry_key(&form.key_encoded) {
        Ok(k) => k,
        Err(_) => {
            return Redirect::to(&format!("/stores/{}", store_name_url)).into_response();
        }
    };

    let store = match build_store(&state, &session).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Store backend unavailable for entry_delete_handler: {e}");
            return Redirect::to(&format!("/stores/{}", store_name_url)).into_response();
        }
    };

    if let Err(e) = store.delete(&store_name, &key).await {
        tracing::error!("store delete failed for {}/{:?}: {e}", store_name, key);
        return Redirect::to(&format!("/stores/{}?delete_error=1", store_name_url)).into_response();
    }

    let target = if form.origin == "store" {
        format!("/stores/{}?deleted=1", store_name_url)
    } else {
        "/stores?deleted=1".to_string()
    };
    Redirect::to(&target).into_response()
}

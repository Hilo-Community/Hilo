//! `GET /v1/tasks/:id/events`: cursor-resumable SSE (`LOT-017`'s "cursor SSE reconnection &
//! zero implicit sync" blocking test). A client that disconnects and reconnects with
//! `Last-Event-ID: <cursor>` gets exactly the events after that cursor — no replay from the
//! start, no implicit sync of anything else.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use uuid::Uuid;

use crate::db;
use crate::state::AppState;
use crate::tenant_mw::AuthenticatedTenant;

pub async fn task_events(
    State(state): State<AppState>,
    tenant: AuthenticatedTenant,
    Path(task_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if !db::task_belongs_to_tenant(&state.pool, tenant.tenant_id, task_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        // Looks exactly like "no such task" whether it belongs to another tenant or does not
        // exist at all: never confirm cross-tenant existence.
        return Err(StatusCode::NOT_FOUND);
    }
    let after_cursor: i64 = headers
        .get("Last-Event-ID")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let events = db::task_events_after(&state.pool, tenant.tenant_id, task_id, after_cursor)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let stream = async_stream::stream! {
        for event in events {
            yield Ok(Event::default()
                .id(event.cursor.to_string())
                .event(event.event_type)
                .data(event.payload.to_string()));
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

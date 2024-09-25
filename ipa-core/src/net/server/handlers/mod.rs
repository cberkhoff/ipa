mod echo;
mod query;

use axum::Router;

use crate::net::{http_serde, MpcHttpTransport};

pub fn router(transport: MpcHttpTransport) -> Router {
    echo::router().nest(
        http_serde::query::BASE_AXUM_PATH,
        Router::new()
            .merge(query::query_router(transport.clone()))
            .merge(query::h2h_router(transport)),
    )
}

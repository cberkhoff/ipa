mod echo;
mod query;

use axum::Router;

use crate::net::{http_serde, MpcHttpTransport, ShardHttpTransport};

pub fn ring_router(transport: MpcHttpTransport) -> Router {
    echo::router().nest(
        http_serde::query::BASE_AXUM_PATH,
        Router::new()
            .merge(query::ring_query_router(transport.clone()))
            .merge(query::h2h_router(transport)),
    )
}

pub fn shard_router(transport: ShardHttpTransport) -> Router {
    echo::router().nest(
        http_serde::query::BASE_AXUM_PATH,
        Router::new().merge(query::s2s_router(transport)),
    )
}

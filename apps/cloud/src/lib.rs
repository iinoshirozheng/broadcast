//! The Broadcast shared board on Cloudflare Workers: the `/api` routes over D1 and R2, the
//! static web assets, and the realtime hub that tells clients when the board changed.

mod attachments;
mod auth;
mod board;
mod comments;
mod db;
mod error;
mod http;
mod hub;
mod projects;
mod readme;
mod records;
mod relations;
mod routes;
mod tasks;

use serde_json::json;
use worker::{Context, Env, Method, Request, Response, event};

use crate::board::Board;
use crate::error::Result;

pub use hub::RealtimeHub;

async fn broadcast(env: &Env, revision: serde_json::Value) -> worker::Result<()> {
    let response = hub::send(
        env,
        Method::Post,
        "/broadcast",
        Some(&json!({ "revision": revision })),
    )
    .await?;
    if (200..300).contains(&response.status_code()) {
        Ok(())
    } else {
        Err(worker::Error::RustError(format!(
            "Realtime broadcast failed ({})",
            response.status_code()
        )))
    }
}

async fn handle(mut request: Request, env: Env, ctx: &Context) -> Result<Response> {
    let url = request.url()?;
    let method = request.method();
    if url.path() == "/health" {
        if method != Method::Get {
            return http::method_not_allowed(&["GET"]);
        }
        return http::json(200, &json!({ "status": "ok" }));
    }
    let Some(session) = auth::authenticate(&request, &env)? else {
        return auth::unauthorized();
    };
    let api = url.path().starts_with("/api/");
    let mut response = if api {
        let board = Board {
            db: env.d1("DB")?,
            env: env.clone(),
            actor: session.actor,
        };
        routes::route(&board, &mut request, &url).await?
    } else if let Ok(assets) = env.assets("ASSETS") {
        assets.fetch_request(request).await?
    } else {
        http::json(
            404,
            &json!({ "error": { "code": "NOT_FOUND", "message": "Resource not found" } }),
        )?
    };
    if let Some(cookie) = &session.cookie
        && response.status_code() != 101
    {
        response = http::with_cookie(response, cookie)?;
    }
    if api
        && (200..300).contains(&response.status_code())
        && !matches!(method, Method::Get | Method::Head | Method::Options)
    {
        let revision = routes::global_revision(&env.d1("DB")?).await?;
        ctx.wait_until(async move {
            if let Err(error) = broadcast(&env, revision).await {
                worker::console_error!("{error}");
            }
        });
    }
    Ok(response)
}

#[event(fetch)]
async fn fetch(request: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let response = match handle(request, env, &ctx).await {
        Ok(response) => Ok(response),
        Err(failure) => http::failure_response(failure),
    };
    response
        .and_then(http::with_security_headers)
        .or_else(|_| Response::error("Internal server error", 500))
}

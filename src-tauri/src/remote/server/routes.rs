//! HTTP request routing, static asset lookup and `POST /pair` handling.

use std::borrow::Cow;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{Conn, Context, Shared};
use crate::remote::assets;
use crate::remote::devices::normalize_device_name;
use crate::remote::http::response::Response;
use crate::remote::http::{read_body, read_head, Head, HttpError};
use crate::remote::pairing::PairError;

/// `POST /pair` bodies are tiny JSON; anything bigger is refused.
const MAX_PAIR_BODY: usize = 4096;

pub(super) enum Outcome {
    Reply(Response),
    Upgrade(Head),
    Drop,
}

const fn error_outcome(error: &HttpError) -> Outcome {
    match error {
        HttpError::TooLarge => Outcome::Reply(Response::status(431)),
        HttpError::Malformed => Outcome::Reply(Response::status(400)),
        HttpError::Timeout => Outcome::Reply(Response::status(408)),
        HttpError::Closed | HttpError::Io => Outcome::Drop,
    }
}

pub(super) fn serve(
    context: &Context,
    conn: &mut Conn,
    peer: IpAddr,
    deadline: Instant,
) -> Outcome {
    let socket = &conn.tcp;
    let arm = |remaining: Duration| socket.set_read_timeout(Some(remaining));
    let (head, rest) = match read_head(&mut conn.stream, &arm, deadline) {
        Ok(parsed) => parsed,
        // A stalled or failed handshake gets no HTTP answer.
        Err(_) if conn.stream.is_handshaking() => return Outcome::Drop,
        Err(error) => return error_outcome(&error),
    };
    if !context.allowed.host_ok(&head) {
        return Outcome::Reply(Response::status(403));
    }
    if head.is_websocket_upgrade() {
        if head.method != "GET" || head.path != "/ws" {
            return Outcome::Reply(Response::status(404));
        }
        if !context.allowed.origin_ok(&head) {
            return Outcome::Reply(Response::status(403));
        }
        return Outcome::Upgrade(head);
    }
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", path) => Outcome::Reply(static_asset(path)),
        ("POST", "/pair") => {
            // A browser POST from another site carries its own Origin.
            if head.header("origin").is_some() && !context.allowed.origin_ok(&head) {
                return Outcome::Reply(Response::status(403));
            }
            let length = match head.content_length() {
                Ok(Some(length)) if length <= MAX_PAIR_BODY => length,
                Ok(Some(_)) => return Outcome::Reply(Response::status(413)),
                Ok(None) => return Outcome::Reply(Response::status(411)),
                Err(error) => return error_outcome(&error),
            };
            match read_body(&mut conn.stream, &arm, deadline, rest, length) {
                Ok(body) => Outcome::Reply(pair(&context.shared, &body, peer)),
                Err(error) => error_outcome(&error),
            }
        }
        _ => Outcome::Reply(Response::status(405)),
    }
}

fn static_asset(path: &str) -> Response {
    assets::lookup(path).map_or_else(
        || Response::status(404),
        |asset| Response {
            status: 200,
            content_type: asset.content_type,
            body: Cow::Borrowed(asset.body),
            extra: &[],
        },
    )
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    name: String,
}

/// HTTP status for a refused pairing code.
pub const fn pair_status(error: &PairError) -> u16 {
    match error {
        PairError::Invalid => 403,
        PairError::Expired => 410,
        PairError::RateLimited => 429,
    }
}

pub(super) fn pair(shared: &Shared, body: &[u8], peer: IpAddr) -> Response {
    let Ok(request) = serde_json::from_slice::<PairRequest>(body) else {
        return Response::status(400);
    };
    let Ok(name) = normalize_device_name(&request.name) else {
        return Response::status(400);
    };
    if !(shared.is_current)() {
        return Response::status(503);
    }
    if let Err(error) = shared.pairing.redeem(&request.code, peer, Instant::now()) {
        let response = Response::status(pair_status(&error));
        return if error == PairError::RateLimited {
            response.with_extra(&[("Retry-After", "60")])
        } else {
            response
        };
    }
    match shared.devices.add(&name) {
        Ok((device, token)) => {
            (shared.on_paired)();
            Response::json(
                200,
                serde_json::json!({ "token": token, "deviceId": device.id })
                    .to_string()
                    .into_bytes(),
            )
        }
        Err(error) => {
            eprintln!("[omp-desktop] could not save paired device: {error}");
            Response::status(500)
        }
    }
}

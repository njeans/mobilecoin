// Copyright (c) 2018-2020 MobileCoin Inc.

// Auto-generatd modules (see build.rs)
mod health_api;
mod health_api_grpc;

mod health_service;

use common::logger::{log, o, Logger};
use futures::Future;
use grpcio::{RpcContext, RpcStatus, RpcStatusCode, UnarySink};
use metrics::SVC_COUNTERS;
use rand::Rng;
use std::sync::atomic::{AtomicU64, Ordering};

pub use health_service::{HealthCheckStatus, HealthService};

/// Helper which reduces boilerplate when implementing grpc API traits.
#[inline]
pub fn send_result<T>(
    ctx: RpcContext,
    sink: UnarySink<T>,
    resp: Result<T, RpcStatus>,
    logger: &Logger,
) {
    let logger = logger.clone();
    let mut success = true;

    match resp {
        Ok(ok) => ctx.spawn(sink.success(ok).map_err(move |err| {
            success = false;
            log::error!(logger, "failed to reply: {:?}", err);
        })),
        Err(e) => {
            success = false;
            let f = sink
                .fail(e)
                .map_err(move |err| log::error!(logger, "failed to reply: {:?}", err));
            ctx.spawn(f)
        }
    }
    SVC_COUNTERS.resp(&ctx, success);
}

/// The most common context strings for `report_err_with_code` are `Enclave Error` and
/// database error
#[inline]
pub fn rpc_enclave_err<E: core::fmt::Debug>(err: E, logger: &Logger) -> RpcStatus {
    report_err_with_code(
        "Enclave Error",
        err,
        RpcStatusCode::INVALID_ARGUMENT,
        logger,
    )
}

#[inline]
pub fn rpc_database_err<E: core::fmt::Debug>(err: E, logger: &Logger) -> RpcStatus {
    report_err_with_code("Database Error", err, RpcStatusCode::INTERNAL, logger)
}

/// More general helpers which reduces boilerplate when reporting errors that
/// don't implement the trait -- or, when the type of the error doesn't always
/// indicate what kind of error. For instance deserialization might sometimes be
/// invalid input and sometimes an internal or database error.
#[inline]
pub fn rpc_internal_error<S: ToString, E: core::fmt::Debug>(
    context: S,
    err: E,
    logger: &Logger,
) -> RpcStatus {
    report_err_with_code(context, err, RpcStatusCode::INTERNAL, logger)
}

#[inline]
pub fn rpc_invalid_arg_error<S: ToString, E: core::fmt::Debug>(
    context: S,
    err: E,
    logger: &Logger,
) -> RpcStatus {
    report_err_with_code(context, err, RpcStatusCode::INVALID_ARGUMENT, logger)
}

#[inline]
pub fn rpc_permissions_error<S: ToString, E: core::fmt::Debug>(
    context: S,
    err: E,
    logger: &Logger,
) -> RpcStatus {
    report_err_with_code(context, err, RpcStatusCode::PERMISSION_DENIED, logger)
}

#[inline]
pub fn rpc_out_of_range_error<S: ToString, E: core::fmt::Debug>(
    context: S,
    err: E,
    logger: &Logger,
) -> RpcStatus {
    report_err_with_code(context, err, RpcStatusCode::OUT_OF_RANGE, logger)
}

#[inline]
pub fn report_err_with_code<S: ToString, E: core::fmt::Debug>(
    context: S,
    err: E,
    code: RpcStatusCode,
    logger: &Logger,
) -> RpcStatus {
    let err_str = format!("{}: {:?}", context.to_string(), err);
    log::error!(logger, "{}", err_str);
    RpcStatus::new(code, Some(err_str))
}

/// Converts a serialization Error to an RpcStatus error.
pub fn ser_to_rpc_err(error: mcserial::encode::Error, logger: &Logger) -> RpcStatus {
    rpc_internal_error("Serialization", error, logger)
}

/// Converts a deserialization Error to an RpcStatus error.
pub fn deser_to_rpc_err(error: mcserial::decode::Error, logger: &Logger) -> RpcStatus {
    rpc_internal_error("Deserialization", error, logger)
}

/// Converts an encode Error to an RpcStatus error.
pub fn encode_to_rpc_err(error: mcserial::EncodeError, logger: &Logger) -> RpcStatus {
    rpc_internal_error("Encode", error, logger)
}

/// Converts a decode Error to an RpcStatus error.
pub fn decode_to_rpc_err(error: mcserial::DecodeError, logger: &Logger) -> RpcStatus {
    rpc_internal_error("Decode", error, logger)
}

/// Helper for running a server around an instance of grpc API implementation
/// Can be reused for many endpoints
/// Handles a bunch of grpc boilerplate that was being copy pasted
use grpcio::{Server, Service};

#[inline]
pub fn run_server(
    env: std::sync::Arc<grpcio::Environment>,
    services: Vec<Service>,
    port: u16,
    logger: &Logger,
) -> Server {
    use grpcio::ServerBuilder;

    // FIXME: This should default to localhost and you should have to provide the IP
    let mut server = ServerBuilder::new(env);

    for service in services {
        server = server.register_service(service);
    }

    let mut server = server.bind("0.0.0.0", port).build().unwrap();
    server.start();
    for (host, port) in server.bind_addrs() {
        log::info!(logger, "API listening on {}:{}", host, port);
    }
    server
}

/// A utility method for injecting peer information into a logger, ideally making it easier to
/// debug RPC-related interactions.
pub fn rpc_logger(ctx: &RpcContext, logger: &Logger) -> Logger {
    let hash =
        common::fast_hash(format!("{}{}", *RPC_LOGGER_CLIENT_ID_SEED, ctx.peer()).as_bytes());
    let hash_str = hex_fmt::HexFmt(hash).to_string();

    let request_id = RPC_LOGGER_REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);

    logger.new(o!("rpc_client_id" => hash_str, "rpc_request_id" => request_id))
}

lazy_static::lazy_static! {
    // Generate a random seed at startup so that rpc_client_id hashes are not identifying specific
    // users by leaking IP addresses.
    static ref RPC_LOGGER_CLIENT_ID_SEED: String = {
        let mut rng = rand::thread_rng();
        std::iter::repeat(())
            .map(|()| rng.sample(rand::distributions::Alphanumeric))
            .take(32)
            .collect()
    };

    static ref RPC_LOGGER_REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
}

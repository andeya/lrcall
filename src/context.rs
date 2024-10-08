// Modifications Copyright Andeya Lee 2024
// Based on original source code from Google LLC licensed under MIT
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

//! Provides a request context that carries a deadline and trace context. This context is sent from
//! client to server and is used by the server to enforce response deadlines.

use crate::trace::{self, TraceId};
use opentelemetry::trace::TraceContextExt;
use static_assertions::assert_impl_all;
use std::convert::TryFrom;
use std::time::{Duration, Instant};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// A request context that carries request-scoped information like deadlines and trace information.
/// It is sent from client to server and is used by the server to enforce response deadlines.
///
/// The context should not be stored directly in a server implementation, because the context will
/// be different for each request in scope.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
#[cfg_attr(feature = "serde1", derive(serde::Serialize, serde::Deserialize))]
pub struct Context {
    /// When the client expects the request to be complete by. The server should cancel the request
    /// if it is not complete by this time.
    #[cfg_attr(feature = "serde1", serde(default = "ten_seconds_from_now"))]
    // Serialized as a Duration to prevent clock skew issues.
    #[cfg_attr(feature = "serde1", serde(with = "absolute_to_relative_time"))]
    pub deadline: Instant,
    /// Uniquely identifies requests originating from the same source.
    /// When a service handles a request by making requests itself, those requests should
    /// include the same `trace_id` as that included on the original request. This way,
    /// users can trace related actions across a distributed system.
    pub trace_context: trace::Context,
    /// Indicates the type of the current call.
    pub call_type: CallType,
}

/// Call type
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde1", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum CallType {
    /// Local Procedure Call
    LPC = 0,
    /// Remote Procedure Call
    RPC = 1,
}

#[cfg(feature = "serde1")]
mod absolute_to_relative_time {
    pub use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub use std::time::{Duration, Instant};

    pub fn serialize<S>(deadline: &Instant, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let deadline = deadline.duration_since(Instant::now());
        deadline.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Instant, D::Error>
    where
        D: Deserializer<'de>,
    {
        let deadline = Duration::deserialize(deserializer)?;
        Ok(Instant::now() + deadline)
    }

    #[cfg(test)]
    #[derive(serde::Serialize, serde::Deserialize)]
    struct AbsoluteToRelative(#[serde(with = "self")] Instant);

    #[test]
    fn test_serialize() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(10);
        let serialized_deadline = bincode::serialize(&AbsoluteToRelative(deadline)).unwrap();
        let deserialized_deadline: Duration = bincode::deserialize(&serialized_deadline).unwrap();
        // TODO: how to avoid flakiness?
        assert!(deserialized_deadline > Duration::from_secs(9));
    }

    #[test]
    fn test_deserialize() {
        let deadline = Duration::from_secs(10);
        let serialized_deadline = bincode::serialize(&deadline).unwrap();
        let AbsoluteToRelative(deserialized_deadline) = bincode::deserialize(&serialized_deadline).unwrap();
        // TODO: how to avoid flakiness?
        assert!(deserialized_deadline > Instant::now() + Duration::from_secs(9));
    }
}

assert_impl_all!(Context: Send, Sync);

fn ten_seconds_from_now() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

/// Returns the context for the current request, or a default Context if no request is active.
pub fn current(call_type: CallType) -> Context {
    Context::current(call_type)
}

/// Returns the context for the current LPC request, or a default LPC Context if no request is active.
pub fn lpc_current() -> Context {
    Context::current(CallType::LPC)
}

/// Returns the context for the current RPC request, or a default RPC Context if no request is active.
pub fn rpc_current() -> Context {
    Context::current(CallType::RPC)
}

#[derive(Clone)]
struct Deadline(Instant);

impl Default for Deadline {
    fn default() -> Self {
        Self(ten_seconds_from_now())
    }
}

impl Context {
    /// Returns the context for the current request, or a default Context if no request is active.
    pub fn current(call_type: CallType) -> Self {
        let span = tracing::Span::current();
        Self {
            trace_context: trace::Context::try_from(&span).unwrap_or_else(|_| trace::Context::default()),
            deadline: span.context().get::<Deadline>().cloned().unwrap_or_default().0,
            call_type,
        }
    }

    /// Returns the ID of the request-scoped trace.
    pub fn trace_id(&self) -> &TraceId {
        &self.trace_context.trace_id
    }
}

/// An extension trait for [`tracing::Span`] for propagating lrcall Contexts.
pub(crate) trait SpanExt {
    /// Sets the given context on this span. Newly-created spans will be children of the given
    /// context's trace context.
    fn set_context(&self, context: &Context);
}

impl SpanExt for tracing::Span {
    fn set_context(&self, context: &Context) {
        self.set_parent(
            opentelemetry::Context::new()
                .with_remote_span_context(opentelemetry::trace::SpanContext::new(
                    opentelemetry::trace::TraceId::from(context.trace_context.trace_id),
                    opentelemetry::trace::SpanId::from(context.trace_context.span_id),
                    opentelemetry::trace::TraceFlags::from(context.trace_context.sampling_decision),
                    true,
                    opentelemetry::trace::TraceState::default(),
                ))
                .with_value(Deadline(context.deadline)),
        );
    }
}

//!
//! Tower plumbing for adding tracing instrumentation to an HTTP service stack
//!
//! This is loosely based on tower-http's trace crate but with the tokio-tracing
//! specific bits removed and less generics.
//!
//! For those not familiar with tower:
//!
//! - A Layer produces a Service
//! - A Service can then be called with a request which returns a Future
//! - This Future returns a response which contains a Body
//! - This Body contains the data payload (potentially streamed)
//!

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::ready;
use http::{HeaderValue, Request, Response};
use http_body::SizeHint;
use pin_project::pin_project;
use tower::{Layer, Service};

use observability_deps::tracing::error;
use trace::{span::SpanRecorder, TraceCollector};

use crate::classify::{classify_headers, classify_response, Classification};
use crate::ctx::{RequestLogContext, TraceHeaderParser};
use crate::metrics::{MetricsCollection, MetricsRecorder};

/// `TraceLayer` implements `tower::Layer` and can be used to decorate a
/// `tower::Service` to collect information about requests flowing through it
///
/// Including:
///
/// - Extracting distributed trace context and attaching span context
/// - Collecting count and duration metrics - [RED metrics][1]
///
/// [1]: https://www.weave.works/blog/the-red-method-key-metrics-for-microservices-architecture/
#[derive(Debug, Clone)]
pub struct TraceLayer {
    trace_header_parser: TraceHeaderParser,
    metrics: Arc<MetricsCollection>,
    collector: Option<Arc<dyn TraceCollector>>,
}

impl TraceLayer {
    pub fn new(
        trace_header_parser: TraceHeaderParser,
        metric_registry: Arc<metric::Registry>,
        collector: Option<Arc<dyn TraceCollector>>,
        is_grpc: bool,
    ) -> Self {
        Self {
            trace_header_parser,
            metrics: Arc::new(MetricsCollection::new(metric_registry, is_grpc)),
            collector,
        }
    }
}

impl<S> Layer<S> for TraceLayer {
    type Service = TraceService<S>;

    fn layer(&self, service: S) -> Self::Service {
        TraceService {
            service,
            collector: self.collector.clone(),
            metrics: Arc::clone(&self.metrics),
            trace_header_parser: self.trace_header_parser.clone(),
        }
    }
}

/// TraceService wraps an inner tower::Service and instruments its returned futures
#[derive(Debug, Clone)]
pub struct TraceService<S> {
    service: S,
    trace_header_parser: TraceHeaderParser,
    collector: Option<Arc<dyn TraceCollector>>,
    metrics: Arc<MetricsCollection>,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for TraceService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    ResBody: http_body::Body,
{
    type Response = Response<TracedBody<ResBody>>;
    type Error = S::Error;
    type Future = TracedFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<ReqBody>) -> Self::Future {
        let metrics_recorder = Some(self.metrics.recorder(&request));

        let collector = match self.collector.as_ref() {
            Some(collector) => collector,
            None => {
                return TracedFuture {
                    metrics_recorder,
                    span_recorder: SpanRecorder::new(None),
                    inner: self.service.call(request),
                }
            }
        };

        let span = match self.trace_header_parser.parse(collector, request.headers()) {
            Ok(Some(ctx)) => {
                request
                    .extensions_mut()
                    .insert(RequestLogContext::new(ctx.clone()));

                ctx.sampled.then(|| {
                    let span = ctx.child("IOx");

                    // Add context to request for use by service handlers
                    request.extensions_mut().insert(span.ctx.clone());

                    span
                })
            }
            Ok(None) => None,
            Err(e) => {
                error!(%e, "error extracting trace context from request");
                None
            }
        };

        TracedFuture {
            metrics_recorder,
            span_recorder: SpanRecorder::new(span),
            inner: self.service.call(request),
        }
    }
}

/// `TracedFuture` wraps a future returned by a `tower::Service` and
/// instruments the returned body if any
#[pin_project]
#[derive(Debug)]
pub struct TracedFuture<F> {
    span_recorder: SpanRecorder,
    metrics_recorder: Option<MetricsRecorder>,
    #[pin]
    inner: F,
}

impl<F, ResBody, Error> Future for TracedFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, Error>>,
    ResBody: http_body::Body,
{
    type Output = Result<Response<TracedBody<ResBody>>, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result: Result<Response<ResBody>, Error> =
            ready!(self.as_mut().project().inner.poll(cx));

        let projected = self.as_mut().project();
        let span_recorder = projected.span_recorder;
        let mut metrics_recorder = projected.metrics_recorder.take().unwrap();
        match &result {
            Ok(response) => match classify_response(response) {
                (_, Classification::Ok) => match response.body().is_end_stream() {
                    true => {
                        metrics_recorder.set_classification(Classification::Ok);
                        span_recorder.ok("request processed with empty response")
                    }
                    false => span_recorder.event("request processed"),
                },
                (error, c) => {
                    metrics_recorder.set_classification(c);
                    span_recorder.error(error);
                }
            },
            Err(_) => {
                metrics_recorder.set_classification(Classification::ServerErr);
                span_recorder.error("error processing request")
            }
        }

        match result {
            Ok(mut response) => {
                // add trace-id header to the response, if we have one
                let span_recorder = self.as_mut().project().span_recorder.take();
                if let Some(trace_id) = span_recorder.span().map(|span| span.ctx.trace_id) {
                    // format as hex
                    let trace_id = HeaderValue::from_str(&format!("{:x}", trace_id.get())).unwrap();
                    response.headers_mut().insert("trace-id", trace_id);
                }

                Poll::Ready(Ok(response.map(|body| TracedBody {
                    span_recorder,
                    inner: body,
                    metrics_recorder,
                })))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

/// `TracedBody` wraps a `http_body::Body` and instruments it
#[pin_project]
#[derive(Debug)]
pub struct TracedBody<B> {
    span_recorder: SpanRecorder,
    metrics_recorder: MetricsRecorder,
    #[pin]
    inner: B,
}

impl<B: http_body::Body> http_body::Body for TracedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let maybe_result = ready!(self.as_mut().project().inner.poll_data(cx));
        let result = match maybe_result {
            Some(result) => result,
            None => return Poll::Ready(None),
        };

        let projected = self.as_mut().project();
        let span_recorder = projected.span_recorder;
        let metrics_recorder = projected.metrics_recorder;
        match &result {
            Ok(_) => match projected.inner.is_end_stream() {
                true => {
                    metrics_recorder.set_classification(Classification::Ok);
                    span_recorder.ok("returned body data and no trailers")
                }
                false => span_recorder.event("returned body data"),
            },
            Err(_) => {
                metrics_recorder.set_classification(Classification::ServerErr);
                span_recorder.error("error getting body");
            }
        }
        Poll::Ready(Some(result))
    }

    fn poll_trailers(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::header::HeaderMap>, Self::Error>> {
        let result: Result<Option<http::header::HeaderMap>, Self::Error> =
            ready!(self.as_mut().project().inner.poll_trailers(cx));

        let projected = self.as_mut().project();
        let span_recorder = projected.span_recorder;
        let metrics_recorder = projected.metrics_recorder;
        match &result {
            Ok(headers) => match classify_headers(headers.as_ref()) {
                (_, Classification::Ok) => {
                    metrics_recorder.set_classification(Classification::Ok);
                    span_recorder.ok("returned trailers")
                }
                (error, c) => {
                    metrics_recorder.set_classification(c);
                    span_recorder.error(error)
                }
            },
            Err(_) => {
                metrics_recorder.set_classification(Classification::ServerErr);
                span_recorder.error("error getting trailers")
            }
        }

        Poll::Ready(result)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

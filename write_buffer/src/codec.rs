//! Encode/Decode for messages

use crate::core::WriteBufferError;
use data_types::{NonEmptyString, PartitionKey, Sequence};
use dml::{DmlDelete, DmlMeta, DmlOperation, DmlWrite};
use generated_types::{
    google::FromOptionalField,
    influxdata::iox::{
        delete::v1::DeletePayload,
        write_buffer::v1::{write_buffer_payload::Payload, WriteBufferPayload},
    },
};
use http::{HeaderMap, HeaderValue};
use iox_time::Time;
use mutable_batch_pb::decode::decode_database_batch;
use prost::Message;
use std::{borrow::Cow, sync::Arc};
use trace::{ctx::SpanContext, TraceCollector};
use trace_http::ctx::{format_jaeger_trace_context, TraceHeaderParser};

/// Pbdata based content type
pub const CONTENT_TYPE_PROTOBUF: &str =
    r#"application/x-protobuf; schema="influxdata.iox.write_buffer.v1.WriteBufferPayload""#;

/// Message header that determines message content type.
pub const HEADER_CONTENT_TYPE: &str = "content-type";

/// Message header for tracing context.
pub const HEADER_TRACE_CONTEXT: &str = "uber-trace-id";

/// Message header for namespace.
pub const HEADER_NAMESPACE: &str = "iox-namespace";

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ContentType {
    Protobuf,
}

/// IOx-specific headers attached to every write buffer message.
#[derive(Debug)]
pub struct IoxHeaders {
    content_type: ContentType,
    span_context: Option<SpanContext>,
    namespace: String,
}

impl IoxHeaders {
    /// Create new headers with sane default values and given span context.
    pub fn new(
        content_type: ContentType,
        span_context: Option<SpanContext>,
        namespace: String,
    ) -> Self {
        Self {
            content_type,
            span_context,
            namespace,
        }
    }

    /// Creates a new IoxHeaders from an iterator of headers
    pub fn from_headers(
        headers: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<[u8]>)>,
        trace_collector: Option<&Arc<dyn TraceCollector>>,
    ) -> Result<Self, WriteBufferError> {
        let mut span_context = None;
        let mut content_type = None;
        let mut namespace = None;

        for (name, value) in headers {
            let name = name.as_ref();

            if name.eq_ignore_ascii_case(HEADER_CONTENT_TYPE) {
                content_type = match std::str::from_utf8(value.as_ref()) {
                    Ok(CONTENT_TYPE_PROTOBUF) => Some(ContentType::Protobuf),
                    Ok(c) => {
                        return Err(WriteBufferError::invalid_data(format!(
                            "Unknown message format: {}",
                            c
                        )))
                    }
                    Err(e) => {
                        return Err(WriteBufferError::invalid_data(format!(
                            "Error decoding content type header: {}",
                            e
                        )))
                    }
                };
            }

            if let Some(trace_collector) = trace_collector {
                if name.eq_ignore_ascii_case(HEADER_TRACE_CONTEXT) {
                    if let Ok(header_value) = HeaderValue::from_bytes(value.as_ref()) {
                        let mut headers = HeaderMap::new();
                        headers.insert(HEADER_TRACE_CONTEXT, header_value);

                        let parser = TraceHeaderParser::new()
                            .with_jaeger_trace_context_header_name(HEADER_TRACE_CONTEXT);

                        span_context = match parser.parse(trace_collector, &headers) {
                            Ok(None) => None,
                            Ok(Some(ctx)) => ctx.sampled.then(|| ctx),
                            Err(e) => {
                                return Err(WriteBufferError::invalid_data(format!(
                                    "Error decoding trace context: {}",
                                    e
                                )))
                            }
                        };
                    }
                }
            }

            if name.eq_ignore_ascii_case(HEADER_NAMESPACE) {
                namespace = Some(String::from_utf8(value.as_ref().to_vec()).map_err(|e| {
                    WriteBufferError::invalid_data(format!(
                        "Error decoding namespace header: {}",
                        e
                    ))
                })?);
            }
        }

        let content_type =
            content_type.ok_or_else(|| WriteBufferError::invalid_data("No content type header"))?;

        Ok(Self {
            content_type,
            span_context,
            namespace: namespace.unwrap_or_default(),
        })
    }

    /// Gets the content type
    #[allow(dead_code)] // this function is only used in optionally-compiled kafka code
    pub fn content_type(&self) -> ContentType {
        self.content_type
    }

    /// Gets the span context if any
    #[allow(dead_code)] // this function is only used in optionally-compiled kafka code
    pub fn span_context(&self) -> Option<&SpanContext> {
        self.span_context.as_ref()
    }

    /// Returns the header map to encode
    pub fn headers(&self) -> impl Iterator<Item = (&str, Cow<'static, str>)> + '_ {
        let content_type = match self.content_type {
            ContentType::Protobuf => CONTENT_TYPE_PROTOBUF.into(),
        };

        std::iter::once((HEADER_CONTENT_TYPE, content_type))
            .chain(
                self.span_context
                    .as_ref()
                    .map(|ctx| {
                        (
                            HEADER_TRACE_CONTEXT,
                            format_jaeger_trace_context(ctx).into(),
                        )
                    })
                    .into_iter(),
            )
            .chain(std::iter::once((
                HEADER_NAMESPACE,
                self.namespace.clone().into(),
            )))
    }
}

/// Decode a message payload
pub fn decode(
    data: &[u8],
    headers: IoxHeaders,
    sequence: Sequence,
    producer_ts: Time,
    bytes_read: usize,
) -> Result<DmlOperation, WriteBufferError> {
    match headers.content_type {
        ContentType::Protobuf => {
            let meta = DmlMeta::sequenced(sequence, producer_ts, headers.span_context, bytes_read);

            let payload: WriteBufferPayload = prost::Message::decode(data)
                .map_err(|e| format!("failed to decode WriteBufferPayload: {}", e))?;

            let payload = payload.payload.ok_or_else(|| "no payload".to_string())?;

            match payload {
                Payload::Write(write) => {
                    let tables = decode_database_batch(&write).map_err(|e| {
                        WriteBufferError::invalid_data(format!(
                            "failed to decode database batch: {}",
                            e
                        ))
                    })?;

                    let partition_key = if write.partition_key.is_empty() {
                        None
                    } else {
                        Some(PartitionKey::from(write.partition_key))
                    };

                    Ok(DmlOperation::Write(DmlWrite::new(
                        headers.namespace,
                        tables,
                        partition_key,
                        meta,
                    )))
                }
                Payload::Delete(delete) => {
                    let predicate = delete
                        .predicate
                        .required("predicate")
                        .map_err(WriteBufferError::invalid_data)?;

                    Ok(DmlOperation::Delete(DmlDelete::new(
                        headers.namespace,
                        predicate,
                        NonEmptyString::new(delete.table_name),
                        meta,
                    )))
                }
            }
        }
    }
}

/// Encodes a [`DmlOperation`] as a protobuf [`WriteBufferPayload`]
pub fn encode_operation(
    db_name: &str,
    operation: &DmlOperation,
    buf: &mut Vec<u8>,
) -> Result<(), WriteBufferError> {
    let payload = match operation {
        DmlOperation::Write(write) => {
            let batch = mutable_batch_pb::encode::encode_write(db_name, write);
            Payload::Write(batch)
        }
        DmlOperation::Delete(delete) => Payload::Delete(DeletePayload {
            db_name: db_name.to_string(),
            table_name: delete
                .table_name()
                .map(ToString::to_string)
                .unwrap_or_default(),
            predicate: Some(delete.predicate().clone().into()),
        }),
    };

    let payload = WriteBufferPayload {
        payload: Some(payload),
    };

    payload.encode(buf).map_err(WriteBufferError::invalid_input)
}

#[cfg(test)]
mod tests {
    use trace::RingBufferTraceCollector;

    use crate::core::test_utils::assert_span_context_eq_or_linked;

    use super::*;

    #[test]
    fn headers_roundtrip() {
        let collector: Arc<dyn TraceCollector> = Arc::new(RingBufferTraceCollector::new(5));

        let span_context_parent = SpanContext::new(Arc::clone(&collector));
        let span_context = span_context_parent.child("foo").ctx;
        let iox_headers1 = IoxHeaders::new(
            ContentType::Protobuf,
            Some(span_context),
            "namespace".to_owned(),
        );

        let encoded: Vec<_> = iox_headers1
            .headers()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let iox_headers2 = IoxHeaders::from_headers(encoded, Some(&collector)).unwrap();

        assert_eq!(iox_headers1.content_type, iox_headers2.content_type);
        assert_span_context_eq_or_linked(
            iox_headers1.span_context.as_ref().unwrap(),
            iox_headers2.span_context.as_ref().unwrap(),
            vec![],
        );
        assert_eq!(iox_headers1.namespace, iox_headers2.namespace);
    }

    #[test]
    fn headers_case_handling() {
        let collector: Arc<dyn TraceCollector> = Arc::new(RingBufferTraceCollector::new(5));

        let headers = vec![
            ("conTent-Type", CONTENT_TYPE_PROTOBUF),
            ("uber-trace-id", "1:2:3:1"),
            ("uber-trace-ID", "5:6:7:1"),
            ("iOx-Namespace", "namespace"),
        ];

        let actual = IoxHeaders::from_headers(headers.into_iter(), Some(&collector)).unwrap();
        assert_eq!(actual.content_type, ContentType::Protobuf);

        let span_context = actual.span_context.unwrap();
        assert_eq!(span_context.trace_id.get(), 5);
        assert_eq!(span_context.span_id.get(), 6);

        assert_eq!(actual.namespace, "namespace");
    }

    #[test]
    fn headers_no_trace_collector_on_consumer_side() {
        let collector: Arc<dyn TraceCollector> = Arc::new(RingBufferTraceCollector::new(5));

        let span_context = SpanContext::new(Arc::clone(&collector));

        let iox_headers1 = IoxHeaders::new(
            ContentType::Protobuf,
            Some(span_context),
            "namespace".to_owned(),
        );

        let encoded: Vec<_> = iox_headers1
            .headers()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let iox_headers2 = IoxHeaders::from_headers(encoded, None).unwrap();

        assert!(iox_headers2.span_context.is_none());
    }
}

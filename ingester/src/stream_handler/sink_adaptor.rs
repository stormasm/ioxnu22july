//! Compatibility layer providing a [`DmlSink`] impl for [`IngesterData`].

use super::DmlSink;
use crate::{data::IngesterData, lifecycle::LifecycleHandleImpl};
use async_trait::async_trait;
use data_types::SequencerId;
use dml::DmlOperation;
use std::sync::Arc;

/// Provides a [`DmlSink`] implementation for a [`IngesterData`] instance.
#[derive(Debug)]
pub struct IngestSinkAdaptor {
    ingest_data: Arc<IngesterData>,
    lifecycle_handle: LifecycleHandleImpl,
    sequencer_id: SequencerId,
}

impl IngestSinkAdaptor {
    /// Wrap an [`IngesterData`] in an adaptor layer to provide a [`DmlSink`]
    /// implementation.
    pub fn new(
        ingest_data: Arc<IngesterData>,
        lifecycle_handle: LifecycleHandleImpl,
        sequencer_id: SequencerId,
    ) -> Self {
        Self {
            ingest_data,
            lifecycle_handle,
            sequencer_id,
        }
    }
}

#[async_trait]
impl DmlSink for IngestSinkAdaptor {
    async fn apply(&self, op: DmlOperation) -> Result<bool, crate::data::Error> {
        self.ingest_data
            .buffer_operation(self.sequencer_id, op, &self.lifecycle_handle)
            .await
    }
}

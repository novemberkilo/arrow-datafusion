// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Simple iterator over batches for use in testing

use async_trait::async_trait;
use std::{
    any::Any,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::Barrier;

use arrow::{
    datatypes::{DataType, Field, Schema, SchemaRef},
    error::{ArrowError, Result as ArrowResult},
    record_batch::RecordBatch,
};
use futures::Stream;

use crate::physical_plan::{
    common, DisplayFormatType, ExecutionPlan, Partitioning, RecordBatchStream,
    SendableRecordBatchStream, Statistics,
};
use crate::{
    error::{DataFusionError, Result},
    physical_plan::stream::RecordBatchReceiverStream,
};

/// Index into the data that has been returned so far
#[derive(Debug, Default, Clone)]
pub struct BatchIndex {
    inner: std::sync::Arc<std::sync::Mutex<usize>>,
}

impl BatchIndex {
    /// Return the current index
    pub fn value(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        *inner
    }

    // increment the current index by one
    pub fn incr(&self) {
        let mut inner = self.inner.lock().unwrap();
        *inner += 1;
    }
}

/// Iterator over batches
#[derive(Debug, Default)]
pub(crate) struct TestStream {
    /// Vector of record batches
    data: Vec<RecordBatch>,
    /// Index into the data that has been returned so far
    index: BatchIndex,
}

impl TestStream {
    /// Create an iterator for a vector of record batches. Assumes at
    /// least one entry in data (for the schema)
    pub fn new(data: Vec<RecordBatch>) -> Self {
        Self {
            data,
            ..Default::default()
        }
    }

    /// Return a handle to the index counter for this stream
    pub fn index(&self) -> BatchIndex {
        self.index.clone()
    }
}

impl Stream for TestStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let next_batch = self.index.value();

        Poll::Ready(if next_batch < self.data.len() {
            let next_batch = self.index.value();
            self.index.incr();
            Some(Ok(self.data[next_batch].clone()))
        } else {
            None
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.data.len(), Some(self.data.len()))
    }
}

impl RecordBatchStream for TestStream {
    /// Get the schema
    fn schema(&self) -> SchemaRef {
        self.data[0].schema()
    }
}

/// A Mock ExecutionPlan that can be used for writing tests of other ExecutionPlans
///
#[derive(Debug)]
pub struct MockExec {
    /// the results to send back
    data: Vec<ArrowResult<RecordBatch>>,
    schema: SchemaRef,
}

impl MockExec {
    /// Create a new exec with a single partition that returns the
    /// record batches in this Exec. Note the batches are not produced
    /// immediately (the caller has to actually yield and another task
    /// must run) to ensure any poll loops are correct.
    pub fn new(data: Vec<ArrowResult<RecordBatch>>, schema: SchemaRef) -> Self {
        Self { data, schema }
    }
}

#[async_trait]
impl ExecutionPlan for MockExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(1)
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        unimplemented!()
    }

    fn with_new_children(
        &self,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        unimplemented!()
    }

    /// Returns a stream which yields data
    async fn execute(&self, partition: usize) -> Result<SendableRecordBatchStream> {
        assert_eq!(partition, 0);

        // Result doesn't implement clone, so do it ourself
        let data: Vec<_> = self
            .data
            .iter()
            .map(|r| match r {
                Ok(batch) => Ok(batch.clone()),
                Err(e) => Err(clone_error(e)),
            })
            .collect();

        let (tx, rx) = tokio::sync::mpsc::channel(2);

        // task simply sends data in order but in a separate
        // thread (to ensure the batches are not available without the
        // DelayedStream yielding).
        tokio::task::spawn(async move {
            for batch in data {
                println!("Sending batch via delayed stream");
                if let Err(e) = tx.send(batch).await {
                    println!("ERROR batch via delayed stream: {}", e);
                }
            }
        });

        // returned stream simply reads off the rx stream
        Ok(RecordBatchReceiverStream::create(&self.schema, rx))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(f, "MockExec")
            }
        }
    }

    // Panics if one of the batches is an error
    fn statistics(&self) -> Statistics {
        let data: ArrowResult<Vec<_>> = self
            .data
            .iter()
            .map(|r| match r {
                Ok(batch) => Ok(batch.clone()),
                Err(e) => Err(clone_error(e)),
            })
            .collect();

        let data = data.unwrap();

        common::compute_record_batch_statistics(&[data], &self.schema, None)
    }
}

fn clone_error(e: &ArrowError) -> ArrowError {
    use ArrowError::*;
    match e {
        ComputeError(msg) => ComputeError(msg.to_string()),
        _ => unimplemented!(),
    }
}

/// A Mock ExecutionPlan that does not start producing input until a
/// barrier is called
///
#[derive(Debug)]
pub struct BarrierExec {
    /// partitions to send back
    data: Vec<Vec<RecordBatch>>,
    schema: SchemaRef,

    /// all streams wait on this barrier to produce
    barrier: Arc<Barrier>,
}

impl BarrierExec {
    /// Create a new exec with some number of partitions.
    pub fn new(data: Vec<Vec<RecordBatch>>, schema: SchemaRef) -> Self {
        // wait for all streams and the input
        let barrier = Arc::new(Barrier::new(data.len() + 1));
        Self {
            data,
            schema,
            barrier,
        }
    }

    /// wait until all the input streams and this function is ready
    pub async fn wait(&self) {
        println!("BarrierExec::wait waiting on barrier");
        self.barrier.wait().await;
        println!("BarrierExec::wait done waiting");
    }
}

#[async_trait]
impl ExecutionPlan for BarrierExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(self.data.len())
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        unimplemented!()
    }

    fn with_new_children(
        &self,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        unimplemented!()
    }

    /// Returns a stream which yields data
    async fn execute(&self, partition: usize) -> Result<SendableRecordBatchStream> {
        assert!(partition < self.data.len());

        let (tx, rx) = tokio::sync::mpsc::channel(2);

        // task simply sends data in order after barrier is reached
        let data = self.data[partition].clone();
        let b = self.barrier.clone();
        tokio::task::spawn(async move {
            println!("Partition {} waiting on barrier", partition);
            b.wait().await;
            for batch in data {
                println!("Partition {} sending batch", partition);
                if let Err(e) = tx.send(Ok(batch)).await {
                    println!("ERROR batch via barrier stream stream: {}", e);
                }
            }
        });

        // returned stream simply reads off the rx stream
        Ok(RecordBatchReceiverStream::create(&self.schema, rx))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(f, "BarrierExec")
            }
        }
    }

    fn statistics(&self) -> Statistics {
        common::compute_record_batch_statistics(&self.data, &self.schema, None)
    }
}

/// A mock execution plan that errors on a call to execute
#[derive(Debug)]
pub struct ErrorExec {
    schema: SchemaRef,
}
impl ErrorExec {
    pub fn new() -> Self {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "dummy",
            DataType::Int64,
            true,
        )]));
        Self { schema }
    }
}

#[async_trait]
impl ExecutionPlan for ErrorExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(1)
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        unimplemented!()
    }

    fn with_new_children(
        &self,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        unimplemented!()
    }

    /// Returns a stream which yields data
    async fn execute(&self, partition: usize) -> Result<SendableRecordBatchStream> {
        Err(DataFusionError::Internal(format!(
            "ErrorExec, unsurprisingly, errored in partition {}",
            partition
        )))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(f, "ErrorExec")
            }
        }
    }

    fn statistics(&self) -> Statistics {
        Statistics::default()
    }
}

/// A mock execution plan that simply returns the provided statistics
#[derive(Debug, Clone)]
pub struct StatisticsExec {
    stats: Statistics,
    schema: Arc<Schema>,
}
impl StatisticsExec {
    pub fn new(stats: Statistics, schema: Schema) -> Self {
        assert!(
            stats
                .column_statistics
                .as_ref()
                .map(|cols| cols.len() == schema.fields().len())
                .unwrap_or(true),
            "if defined, the column statistics vector length should be the number of fields"
        );
        Self {
            stats,
            schema: Arc::new(schema),
        }
    }
}
#[async_trait]
impl ExecutionPlan for StatisticsExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(2)
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        &self,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.is_empty() {
            Ok(Arc::new(self.clone()))
        } else {
            Err(DataFusionError::Internal(
                "Children cannot be replaced in CustomExecutionPlan".to_owned(),
            ))
        }
    }

    async fn execute(&self, _partition: usize) -> Result<SendableRecordBatchStream> {
        unimplemented!("This plan only serves for testing statistics")
    }

    fn statistics(&self) -> Statistics {
        self.stats.clone()
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(
                    f,
                    "StatisticsExec: col_count={}, row_count={:?}",
                    self.schema.fields().len(),
                    self.stats.num_rows,
                )
            }
        }
    }
}

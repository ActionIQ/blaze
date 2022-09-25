use std::any::Any;
use std::fmt::{Debug, Formatter};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::Result;
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::PhysicalSortExpr;
use datafusion::physical_plan::{DisplayFormatType, ExecutionPlan, Partitioning, RecordBatchStream, SendableRecordBatchStream, Statistics};
use datafusion::physical_plan::metrics::{BaselineMetrics, ExecutionPlanMetricsSet};
use futures::{Stream, StreamExt};
use crate::DataFusionError;

#[derive(Debug)]
pub struct LimitExec {
    input: Arc<dyn ExecutionPlan>,
    limit: u64,
    pub metrics: ExecutionPlanMetricsSet,
}

impl LimitExec {
    pub fn new(input: Arc<dyn ExecutionPlan>, limit: u64) -> Self {
        Self {
            input,
            limit,
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }
}

impl ExecutionPlan for LimitExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }

    fn output_partitioning(&self) -> Partitioning {
        self.input.output_partitioning()
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        self.input.output_ordering()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }

    fn with_new_children(self: Arc<Self>, children: Vec<Arc<dyn ExecutionPlan>>) -> Result<Arc<dyn ExecutionPlan>> {
        match children.len() {
            1 => Ok(Arc::new(Self::new(
                children[0].clone(),
                self.limit,
            ))),
            _ => Err(DataFusionError::Internal(
                "LimitExec wrong number of children".to_string(),
            )),
        }
    }

    fn execute(&self, partition: usize, context: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
        let input_stream = self.input.execute(partition, context)?;
        Ok(Box::pin(LimitStream {
            input_stream,
            limit: self.limit,
            cur: 0,
            baseline_metrics: BaselineMetrics::new(&self.metrics, partition),
        }))
    }

    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "LimitExec(limit={})", self.limit)
    }

    fn statistics(&self) -> Statistics {
        todo!()
    }
}

struct LimitStream {
    input_stream: SendableRecordBatchStream,
    limit: u64,
    cur: u64,
    baseline_metrics: BaselineMetrics,
}

impl RecordBatchStream for LimitStream {
    fn schema(&self) -> SchemaRef {
        self.input_stream.schema()
    }
}

impl Stream for LimitStream {
    type Item = datafusion::arrow::error::Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let rest = self.limit.saturating_sub(self.cur) as usize;
        if rest == 0 {
            return Poll::Ready(None);
        }

        match self.input_stream.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(Some(Ok(batch))) => {
                self.baseline_metrics.record_poll(
                    Poll::Ready(Some(Ok(
                        if batch.num_rows() <= rest {
                            batch
                        } else {
                            batch.slice(0, rest)
                        }
                    )))
                )
            },
        }
    }
}
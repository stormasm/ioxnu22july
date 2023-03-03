//! Query planner wrapper for use in IOx services
use std::sync::Arc;

use datafusion::physical_plan::ExecutionPlan;
use iox_query::{
    exec::IOxSessionContext,
    frontend::{influxrpc::InfluxRpcPlanner, sql::SqlQueryPlanner},
    plan::{fieldlist::FieldListPlan, seriesset::SeriesSetPlans, stringset::StringSetPlan},
    Aggregate, QueryDatabase, WindowDuration,
};

pub use datafusion::error::{DataFusionError as Error, Result};
use predicate::rpc_predicate::InfluxRpcPredicate;

/// Query planner that plans queries on a separate threadpool.
///
/// Query planning was, at time of writing, a single threaded
/// affair. In order to avoid tying up the tokio executor that is
/// handling API requests, we plan queries using a separate thread
/// pool.
pub struct Planner {
    /// Executors (whose threadpool to use)
    ctx: IOxSessionContext,
}

impl Planner {
    /// Create a new planner that will plan queries using the provided context
    pub fn new(ctx: &IOxSessionContext) -> Self {
        Self {
            ctx: ctx.child_ctx("Planner"),
        }
    }

    /// Plan a SQL query against the data in `database`, and return a
    /// DataFusion physical execution plan.
    pub async fn sql(&self, query: impl Into<String> + Send) -> Result<Arc<dyn ExecutionPlan>> {
        let planner = SqlQueryPlanner::new();
        let query = query.into();
        let ctx = self.ctx.child_ctx("planner sql");

        self.ctx
            .run(async move { planner.query(&query, &ctx).await })
            .await
    }

    /// Creates a plan as described on
    /// [`InfluxRpcPlanner::table_names`], on a separate threadpool
    pub async fn table_names<D>(
        &self,
        database: Arc<D>,
        predicate: InfluxRpcPredicate,
    ) -> Result<StringSetPlan>
    where
        D: QueryDatabase + 'static,
    {
        let planner = InfluxRpcPlanner::new(self.ctx.child_ctx("planner table_names"));

        self.ctx
            .run(async move {
                planner
                    .table_names(database.as_ref(), predicate)
                    .await
                    .map_err(|e| Error::Plan(format!("table_names error: {}", e)))
            })
            .await
    }

    /// Creates a plan as described on
    /// [`InfluxRpcPlanner::tag_keys`], on a separate threadpool
    pub async fn tag_keys<D>(
        &self,
        database: Arc<D>,
        predicate: InfluxRpcPredicate,
    ) -> Result<StringSetPlan>
    where
        D: QueryDatabase + 'static,
    {
        let planner = InfluxRpcPlanner::new(self.ctx.child_ctx("planner tag_keys"));

        self.ctx
            .run(async move {
                planner
                    .tag_keys(database.as_ref(), predicate)
                    .await
                    .map_err(|e| Error::Plan(format!("tag_keys error: {}", e)))
            })
            .await
    }

    /// Creates a plan as described on
    /// [`InfluxRpcPlanner::tag_values`], on a separate threadpool
    pub async fn tag_values<D>(
        &self,
        database: Arc<D>,
        tag_name: impl Into<String> + Send,
        predicate: InfluxRpcPredicate,
    ) -> Result<StringSetPlan>
    where
        D: QueryDatabase + 'static,
    {
        let tag_name = tag_name.into();
        let planner = InfluxRpcPlanner::new(self.ctx.child_ctx("planner tag_values"));

        self.ctx
            .run(async move {
                planner
                    .tag_values(database.as_ref(), &tag_name, predicate)
                    .await
                    .map_err(|e| Error::Plan(format!("tag_values error: {}", e)))
            })
            .await
    }

    /// Creates a plan as described on
    /// [`InfluxRpcPlanner::field_columns`], on a separate threadpool
    pub async fn field_columns<D>(
        &self,
        database: Arc<D>,
        predicate: InfluxRpcPredicate,
    ) -> Result<FieldListPlan>
    where
        D: QueryDatabase + 'static,
    {
        let planner = InfluxRpcPlanner::new(self.ctx.child_ctx("planner field_columns"));

        self.ctx
            .run(async move {
                planner
                    .field_columns(database.as_ref(), predicate)
                    .await
                    .map_err(|e| Error::Plan(format!("field_columns error: {}", e)))
            })
            .await
    }

    /// Creates a plan as described on
    /// [`InfluxRpcPlanner::read_filter`], on a separate threadpool
    pub async fn read_filter<D>(
        &self,
        database: Arc<D>,
        predicate: InfluxRpcPredicate,
    ) -> Result<SeriesSetPlans>
    where
        D: QueryDatabase + 'static,
    {
        let planner = InfluxRpcPlanner::new(self.ctx.child_ctx("planner read_filter"));

        self.ctx
            .run(async move {
                planner
                    .read_filter(database.as_ref(), predicate)
                    .await
                    .map_err(|e| Error::Plan(format!("read_filter error: {}", e)))
            })
            .await
    }

    /// Creates a plan as described on
    /// [`InfluxRpcPlanner::read_group`], on a separate threadpool
    pub async fn read_group<D>(
        &self,
        database: Arc<D>,
        predicate: InfluxRpcPredicate,
        agg: Aggregate,
        group_columns: Vec<String>,
    ) -> Result<SeriesSetPlans>
    where
        D: QueryDatabase + 'static,
    {
        let planner = InfluxRpcPlanner::new(self.ctx.child_ctx("planner read_group"));

        self.ctx
            .run(async move {
                planner
                    .read_group(database.as_ref(), predicate, agg, &group_columns)
                    .await
                    .map_err(|e| Error::Plan(format!("read_group error: {}", e)))
            })
            .await
    }

    /// Creates a plan as described on
    /// [`InfluxRpcPlanner::read_window_aggregate`], on a separate threadpool
    pub async fn read_window_aggregate<D>(
        &self,
        database: Arc<D>,
        predicate: InfluxRpcPredicate,
        agg: Aggregate,
        every: WindowDuration,
        offset: WindowDuration,
    ) -> Result<SeriesSetPlans>
    where
        D: QueryDatabase + 'static,
    {
        let planner = InfluxRpcPlanner::new(self.ctx.child_ctx("planner read_window_aggregate"));

        self.ctx
            .run(async move {
                planner
                    .read_window_aggregate(database.as_ref(), predicate, agg, every, offset)
                    .await
                    .map_err(|e| Error::Plan(format!("read_window_aggregate error: {}", e)))
            })
            .await
    }
}

//! This module contains testing scenarios for Delete

use super::{
    util::{make_n_chunks_scenario, ChunkData, DeleteTime, Pred},
    DbScenario, DbSetup,
};
use crate::scenarios::util::all_scenarios_for_one_chunk;
use async_trait::async_trait;
use data_types::{DeleteExpr, DeletePredicate, Op, Scalar, TimestampRange};

// ================================================================================================
// DELETE TEST SETUPS: chunk lp data, how many chunks, their types, how many delete predicates and
// when they happen

#[derive(Debug)]
/// Setup for delete query test with one table and one chunk. All data will be soft deleted in this
/// setup.
pub struct OneDeleteSimpleExprOneChunkDeleteAll {}
#[async_trait]
impl DbSetup for OneDeleteSimpleExprOneChunkDeleteAll {
    async fn make(&self) -> Vec<DbScenario> {
        let partition_key = "1970-01-01T00";
        let table_name = "cpu";

        // chunk data
        let lp_lines = vec!["cpu bar=1 10", "cpu bar=2 20"];

        // delete predicate
        let pred = DeletePredicate {
            range: TimestampRange::new(10, 20),
            exprs: vec![],
        };

        all_scenarios_for_one_chunk(vec![&pred], vec![], lp_lines, table_name, partition_key).await
    }
}

#[derive(Debug)]
/// Setup for delete query test with one table and one chunk
pub struct OneDeleteSimpleExprOneChunk {}
#[async_trait]
impl DbSetup for OneDeleteSimpleExprOneChunk {
    async fn make(&self) -> Vec<DbScenario> {
        let partition_key = "1970-01-01T00";
        let table_name = "cpu";

        // chunk data
        let lp_lines = vec!["cpu bar=1 10", "cpu bar=2 20"];

        // delete predicate
        let pred = DeletePredicate {
            range: TimestampRange::new(0, 15),
            exprs: vec![DeleteExpr::new(
                "bar".to_string(),
                Op::Eq,
                Scalar::F64((1.0).into()),
            )],
        };

        all_scenarios_for_one_chunk(vec![&pred], vec![], lp_lines, table_name, partition_key).await
    }
}

#[derive(Debug)]
/// Setup for many scenarios moving the chunk to different stages. No delete in this case.
pub struct NoDeleteOneChunk {}
#[async_trait]
impl DbSetup for NoDeleteOneChunk {
    async fn make(&self) -> Vec<DbScenario> {
        let partition_key = "1970-01-01T00";
        let table_name = "cpu";
        // chunk data
        let lp_lines = vec![
            "cpu,foo=me bar=1 10",
            "cpu,foo=you bar=2 20",
            "cpu,foo=me bar=1 30",
            "cpu,foo=me bar=1 40",
        ];

        all_scenarios_for_one_chunk(vec![], vec![], lp_lines, table_name, partition_key).await
    }
}

#[derive(Debug)]
/// Setup for multi-expression delete query test with one table and one chunk
pub struct OneDeleteMultiExprsOneChunk {}
#[async_trait]
impl DbSetup for OneDeleteMultiExprsOneChunk {
    async fn make(&self) -> Vec<DbScenario> {
        let partition_key = "1970-01-01T00";
        let table_name = "cpu";
        // chunk data
        let lp_lines = vec![
            "cpu,foo=me bar=1 10", // deleted
            "cpu,foo=you bar=2 20",
            "cpu,foo=me bar=1 30", // deleted
            "cpu,foo=me bar=1 40",
        ];
        // delete predicate
        let pred = DeletePredicate {
            range: TimestampRange::new(0, 30),
            exprs: vec![
                DeleteExpr::new("bar".to_string(), Op::Eq, Scalar::F64((1.0).into())),
                DeleteExpr::new("foo".to_string(), Op::Eq, Scalar::String("me".to_string())),
            ],
        };

        all_scenarios_for_one_chunk(vec![&pred], vec![], lp_lines, table_name, partition_key).await
    }
}

#[derive(Debug)]
/// Setup for multi-expression delete query test with one table and one chunk. Two deletes at
/// different chunk stages.
pub struct TwoDeletesMultiExprsOneChunk {}
#[async_trait]
impl DbSetup for TwoDeletesMultiExprsOneChunk {
    async fn make(&self) -> Vec<DbScenario> {
        // The main purpose of these scenarios is the multi-expression delete predicate is added in
        // the ingester and is moved with chunk moving. Then one more delete after moving.

        // General setup for all scenarios
        let partition_key = "1970-01-01T00";
        let table_name = "cpu";
        // chunk data
        let lp_lines = vec![
            "cpu,foo=me bar=1 10",
            "cpu,foo=you bar=2 20",
            "cpu,foo=me bar=1 30",
            "cpu,foo=me bar=1 40",
        ];
        // delete predicate
        // pred1: delete from cpu where 0 <= time <= 32 and bar = 1 and foo = 'me'
        let pred1 = DeletePredicate {
            range: TimestampRange::new(0, 32),
            exprs: vec![
                DeleteExpr::new("bar".to_string(), Op::Eq, Scalar::F64((1.0).into())),
                DeleteExpr::new("foo".to_string(), Op::Eq, Scalar::String("me".to_string())),
            ],
        };

        // pred2: delete from cpu where 10 <= time <= 40 and bar != 1
        let pred2 = DeletePredicate {
            range: TimestampRange::new(10, 40),
            exprs: vec![DeleteExpr::new(
                "bar".to_string(),
                Op::Ne,
                Scalar::F64((1.0).into()),
            )],
        };

        // build all possible scenarios
        all_scenarios_for_one_chunk(
            vec![&pred1],
            vec![&pred2],
            lp_lines,
            table_name,
            partition_key,
        )
        .await
    }
}

// Three different delete on three different chunks
#[derive(Debug)]
/// Setup for three different delete on three different chunks
pub struct ThreeDeleteThreeChunks {}
#[async_trait]
impl DbSetup for ThreeDeleteThreeChunks {
    async fn make(&self) -> Vec<DbScenario> {
        // General setup for all scenarios
        let partition_key = "1970-01-01T00";
        let table_name = "cpu";

        // chunk1 data
        let lp_lines_1 = vec![
            "cpu,foo=me bar=1 10",  // deleted by pred1
            "cpu,foo=you bar=2 20", // deleted by pred2
            "cpu,foo=me bar=1 30",  // deleted by pred1
            "cpu,foo=me bar=1 40",
        ];
        // delete predicate on chunk 1
        let pred1 = DeletePredicate {
            range: TimestampRange::new(0, 30),
            exprs: vec![
                DeleteExpr::new("bar".to_string(), Op::Eq, Scalar::F64((1.0).into())),
                DeleteExpr::new("foo".to_string(), Op::Eq, Scalar::String("me".to_string())),
            ],
        };

        //chunk 2 data
        let lp_lines_2 = vec![
            "cpu,foo=me bar=1 42",
            "cpu,foo=you bar=3 42", // deleted by pred2
            "cpu,foo=me bar=4 50",
            "cpu,foo=me bar=5 60",
        ];
        // delete predicate on chunk 1 & chunk 2
        let pred2 = DeletePredicate {
            range: TimestampRange::new(20, 45),
            exprs: vec![DeleteExpr::new(
                "foo".to_string(),
                Op::Eq,
                Scalar::String("you".to_string()),
            )],
        };

        // chunk 3 data
        let lp_lines_3 = vec![
            "cpu,foo=me bar=1 62",
            "cpu,foo=you bar=3 70",
            "cpu,foo=me bar=7 80",
            "cpu,foo=me bar=8 90", // deleted by pred3
        ];
        // delete predicate on chunk 3
        let pred3 = DeletePredicate {
            range: TimestampRange::new(75, 95),
            exprs: vec![DeleteExpr::new(
                "bar".to_string(),
                Op::Ne,
                Scalar::F64((7.0).into()),
            )],
        };

        //let preds = vec![&pred1, &pred2, &pred3];
        let preds = vec![
            Pred {
                predicate: &pred1,
                delete_time: DeleteTime::End,
            },
            Pred {
                predicate: &pred2,
                delete_time: DeleteTime::End,
            },
            Pred {
                predicate: &pred3,
                delete_time: DeleteTime::End,
            },
        ];

        // Scenarios
        // All threee deletes will be applied to every chunk but due to their predicates,
        // only appropriate data is deleted
        let scenarios = make_n_chunks_scenario(&[
            ChunkData {
                lp_lines: lp_lines_1,
                preds: preds.clone(),
                delete_table_name: Some(table_name),
                partition_key,
                ..Default::default()
            },
            ChunkData {
                lp_lines: lp_lines_2,
                preds: preds.clone(),
                delete_table_name: Some(table_name),
                partition_key,
                ..Default::default()
            },
            ChunkData {
                lp_lines: lp_lines_3,
                preds,
                delete_table_name: Some(table_name),
                partition_key,
                ..Default::default()
            },
        ])
        .await;

        scenarios
    }
}

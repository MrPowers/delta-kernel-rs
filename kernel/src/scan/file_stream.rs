use std::collections::HashSet;

use super::data_skipping::DataSkippingFilter;
use crate::actions::{parse_actions, Action, ActionType, Add};
use crate::expressions::Expression;
use crate::schema::SchemaRef;
use crate::DeltaResult;

use arrow_array::RecordBatch;
use either::Either;
use tracing::debug;

struct LogReplayScanner {
    filter: Option<DataSkippingFilter>,

    /// A set of (data file path, dv_unique_id) pairs that have been seen thus
    /// far in the log. This is used to filter out files with Remove actions as
    /// well as duplicate entries in the log.
    seen: HashSet<(String, Option<String>)>,
}

impl LogReplayScanner {
    /// Create a new [`LogReplayStream`] instance
    fn new(table_schema: &SchemaRef, predicate: &Option<Expression>) -> Self {
        Self {
            filter: DataSkippingFilter::new(table_schema, predicate),
            seen: Default::default(),
        }
    }

    /// Extract Add actions from a single batch. This will filter out rows that
    /// don't match the predicate and Add actions that have corresponding Remove
    /// actions in the log.
    fn process_batch(
        &mut self,
        actions: &RecordBatch,
        is_log_batch: bool,
    ) -> DeltaResult<Vec<Add>> {
        let filtered_actions = match &self.filter {
            Some(filter) => Some(filter.apply(actions)?),
            None => None,
        };
        let actions = if let Some(filtered) = &filtered_actions {
            filtered
        } else {
            actions
        };

        let required_action_types = if is_log_batch {
            vec![ActionType::Remove, ActionType::Add]
        } else {
            vec![ActionType::Add]
        };

        let adds: Vec<Add> = parse_actions(actions, &required_action_types)?
            .filter_map(|action| match action {
                Action::Add(add)
                    // Note: each (add.path + add.dv_unique_id()) pair has a 
                    // unique Add + Remove pair in the log. For example:
                    // https://github.com/delta-io/delta/blob/master/spark/src/test/resources/delta/table-with-dv-large/_delta_log/00000000000000000001.json
                    if !self
                        .seen
                        .contains(&(add.path.clone(), add.dv_unique_id())) =>
                {
                    debug!("Found file: {}", &add.path);
                    if is_log_batch {
                        self.seen.insert((add.path.clone(), add.dv_unique_id()));
                    }
                    Some(add)
                }
                Action::Remove(remove) => {
                    // Remove actions are only parsed in log batches, so no need to check here.
                    self.seen
                        .insert((remove.path.clone(), remove.dv_unique_id()));
                    None
                }
                _ => None,
            })
            .collect();

        Ok(adds)
    }
}

/// Given an iterator of record batch, bool tuples and a predicate, returns an iterator of [Add]s.
/// THe boolean flag indicates whether the record batch is a log or checkpoint batch.
pub fn log_replay_iter(
    action_iter: impl Iterator<Item = DeltaResult<(RecordBatch, bool)>>,
    table_schema: &SchemaRef,
    predicate: &Option<Expression>,
) -> impl Iterator<Item = DeltaResult<Add>> {
    let mut log_scanner = LogReplayScanner::new(table_schema, predicate);

    action_iter.flat_map(move |actions| match actions {
        Ok((batch, is_log_batch)) => match log_scanner.process_batch(&batch, is_log_batch) {
            Ok(adds) => Either::Left(adds.into_iter().map(Ok)),
            Err(err) => Either::Right(std::iter::once(Err(err))),
        },
        Err(err) => Either::Right(std::iter::once(Err(err))),
    })
}

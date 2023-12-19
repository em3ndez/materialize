// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Reduction dataflow construction.
//!
//! Consult [ReducePlan] documentation for details.

use std::collections::BTreeMap;

use dec::OrderedDecimal;
use differential_dataflow::collection::AsCollection;
use differential_dataflow::difference::{Multiply, Semigroup};
use differential_dataflow::hashable::Hashable;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::arrange::{Arranged, TraceAgent};
use differential_dataflow::trace::cursor::MyTrait;
use differential_dataflow::trace::{Batch, Batcher, Trace, TraceReader};
use differential_dataflow::{Collection, ExchangeData};
use mz_compute_types::plan::reduce::{
    reduction_type, AccumulablePlan, BasicPlan, BucketedPlan, HierarchicalPlan, KeyValPlan,
    MonotonicPlan, ReducePlan, ReductionType,
};
use mz_expr::{AggregateExpr, AggregateFunc, EvalError, MirScalarExpr};
use mz_repr::adt::numeric::{self, Numeric, NumericAgg};
use mz_repr::{Datum, DatumList, DatumVec, Diff, Row, RowArena, SharedRow};
use mz_storage_types::errors::DataflowError;
use mz_timely_util::operator::CollectionExt;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use timely::container::columnation::{Columnation, CopyRegion};
use timely::dataflow::Scope;
use timely::progress::timestamp::Refines;
use timely::progress::Timestamp;
use tracing::warn;

use crate::extensions::arrange::{ArrangementSize, KeyCollection, MzArrange};
use crate::extensions::reduce::{MzReduce, ReduceExt};
use crate::render::context::{CollectionBundle, Context, SpecializedArrangement};
use crate::render::errors::MaybeValidatingRow;
use crate::render::reduce::monoids::{get_monoid, ReductionMonoid};
use crate::render::ArrangementFlavor;
use crate::typedefs::{
    KeySpine, KeyValSpine, RowErrSpine, RowRowArrangement, RowRowSpine, RowSpine, RowValSpine,
};

impl<G, T> Context<G, T>
where
    G: Scope,
    G::Timestamp: Lattice + Refines<T> + Columnation,
    T: Timestamp + Lattice + Columnation,
{
    /// Renders a `MirRelationExpr::Reduce` using various non-obvious techniques to
    /// minimize worst-case incremental update times and memory footprint.
    pub fn render_reduce(
        &mut self,
        input: CollectionBundle<G, T>,
        key_val_plan: KeyValPlan,
        reduce_plan: ReducePlan,
        input_key: Option<Vec<MirScalarExpr>>,
    ) -> CollectionBundle<G, T> {
        input.scope().region_named("Reduce", |inner| {
            let KeyValPlan {
                mut key_plan,
                mut val_plan,
            } = key_val_plan;
            let key_arity = key_plan.projection.len();
            let mut datums = DatumVec::new();
            let (key_val_input, err_input): (
                timely::dataflow::Stream<_, (Result<(Row, Row), DataflowError>, _, _)>,
                _,
            ) = input
                .enter_region(inner)
                .flat_map(input_key.map(|k| (k, None)), || {
                    // Determine the columns we'll need from the row.
                    let mut demand = Vec::new();
                    demand.extend(key_plan.demand());
                    demand.extend(val_plan.demand());
                    demand.sort();
                    demand.dedup();
                    // remap column references to the subset we use.
                    let mut demand_map = BTreeMap::new();
                    for column in demand.iter() {
                        demand_map.insert(*column, demand_map.len());
                    }
                    let demand_map_len = demand_map.len();
                    key_plan.permute(demand_map.clone(), demand_map_len);
                    val_plan.permute(demand_map, demand_map_len);
                    let skips = mz_compute_types::plan::reduce::convert_indexes_to_skips(demand);
                    move |row_datums, time, diff| {
                        let binding = SharedRow::get();
                        let mut row_builder = binding.borrow_mut();
                        let temp_storage = RowArena::new();

                        let mut row_iter = row_datums.drain(..);
                        let mut datums_local = datums.borrow();
                        // Unpack only the demanded columns.
                        for skip in skips.iter() {
                            datums_local.push(row_iter.nth(*skip).unwrap());
                        }

                        // Evaluate the key expressions.
                        let key = match key_plan.evaluate_into(
                            &mut datums_local,
                            &temp_storage,
                            &mut row_builder,
                        ) {
                            Err(e) => {
                                return Some((
                                    Err(DataflowError::from(e)),
                                    time.clone(),
                                    diff.clone(),
                                ))
                            }
                            Ok(key) => key.expect("Row expected as no predicate was used"),
                        };
                        // Evaluate the value expressions.
                        // The prior evaluation may have left additional columns we should delete.
                        datums_local.truncate(skips.len());
                        let val = match val_plan.evaluate_iter(&mut datums_local, &temp_storage) {
                            Err(e) => {
                                return Some((
                                    Err(DataflowError::from(e)),
                                    time.clone(),
                                    diff.clone(),
                                ))
                            }
                            Ok(val) => val.expect("Row expected as no predicate was used"),
                        };
                        row_builder.packer().extend(val);
                        let row = row_builder.clone();
                        Some((Ok((key, row)), time.clone(), diff.clone()))
                    }
                });

            // Demux out the potential errors from key and value selector evaluation.
            let (ok, mut err) = key_val_input
                .as_collection()
                .consolidate_stream()
                .flat_map_fallible("OkErrDemux", Some);

            err = err.concat(&err_input);

            // Render the reduce plan
            self.render_reduce_plan(reduce_plan, ok, err, key_arity)
                .leave_region()
        })
    }

    /// Render a dataflow based on the provided plan.
    ///
    /// The output will be an arrangements that looks the same as if
    /// we just had a single reduce operator computing everything together, and
    /// this arrangement can also be re-used.
    fn render_reduce_plan<S>(
        &self,
        plan: ReducePlan,
        collection: Collection<S, (Row, Row), Diff>,
        err_input: Collection<S, DataflowError, Diff>,
        key_arity: usize,
    ) -> CollectionBundle<S, T>
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        let mut errors = Default::default();
        let arrangement = self.render_reduce_plan_inner(plan, collection, &mut errors, key_arity);
        let errs: KeyCollection<_, _, _> = err_input.concatenate(errors).into();
        CollectionBundle::from_columns(
            0..key_arity,
            ArrangementFlavor::Local(arrangement, errs.mz_arrange("Arrange bundle err")),
        )
    }

    fn render_reduce_plan_inner<S>(
        &self,
        plan: ReducePlan,
        collection: Collection<S, (Row, Row), Diff>,
        errors: &mut Vec<Collection<S, DataflowError, Diff>>,
        key_arity: usize,
    ) -> SpecializedArrangement<S>
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        // TODO(vmarcos): Arrangement specialization here could eventually be extended to keys,
        // not only values (#22103).
        let arrangement = match plan {
            // If we have no aggregations or just a single type of reduction, we
            // can go ahead and render them directly.
            ReducePlan::Distinct => {
                let (arranged_output, errs) = self.dispatch_build_distinct(collection);
                errors.push(errs);
                arranged_output
            }
            ReducePlan::Accumulable(expr) => {
                let (arranged_output, errs) = self.build_accumulable(collection, expr);
                errors.push(errs);
                SpecializedArrangement::RowRow(arranged_output)
            }
            ReducePlan::Hierarchical(HierarchicalPlan::Monotonic(expr)) => {
                let (output, errs) = self.build_monotonic(collection, expr);
                errors.push(errs);
                SpecializedArrangement::RowRow(output)
            }
            ReducePlan::Hierarchical(HierarchicalPlan::Bucketed(expr)) => {
                let (output, errs) = self.build_bucketed(collection, expr);
                if let Some(e) = errs {
                    errors.push(e);
                }
                SpecializedArrangement::RowRow(output)
            }
            ReducePlan::Basic(BasicPlan::Single(index, aggr)) => {
                let (output, errs) = self.build_basic_aggregate(collection, index, &aggr, true);
                errors.push(errs.expect("validation should have occurred as it was requested"));
                SpecializedArrangement::RowRow(output)
            }
            ReducePlan::Basic(BasicPlan::Multiple(aggrs)) => {
                let (output, errs) = self.build_basic_aggregates(collection, aggrs);
                errors.push(errs);
                SpecializedArrangement::RowRow(output)
            }
            // Otherwise, we need to render something different for each type of
            // reduction, and then stitch them together.
            ReducePlan::Collation(expr) => {
                // First, we need to render our constituent aggregations.
                let mut to_collate = vec![];

                for plan in [
                    expr.hierarchical.map(ReducePlan::Hierarchical),
                    expr.accumulable.map(ReducePlan::Accumulable),
                    expr.basic.map(ReducePlan::Basic),
                ]
                .into_iter()
                .flat_map(std::convert::identity)
                {
                    let r#type = ReductionType::try_from(&plan)
                        .expect("only representable reduction types were used above");

                    let arrangement = match self.render_reduce_plan_inner(
                        plan,
                        collection.clone(),
                        errors,
                        key_arity,
                    ) {
                        SpecializedArrangement::RowUnit(_) => {
                            unreachable!(
                                "Unexpected RowUnit arrangement in reduce collation rendering"
                            )
                        }
                        SpecializedArrangement::RowRow(arranged) => arranged,
                    };
                    to_collate.push((r#type, arrangement));
                }

                // Now we need to collate them together.
                let (oks, errs) =
                    self.build_collation(to_collate, expr.aggregate_types, &mut collection.scope());
                errors.push(errs);
                SpecializedArrangement::RowRow(oks)
            }
        };
        arrangement
    }

    /// Build the dataflow to combine arrangements containing results of different
    /// aggregation types into a single arrangement.
    ///
    /// This computes the same thing as a join on the group key followed by shuffling
    /// the values into the correct order. This implementation assumes that all input
    /// arrangements present values in a way that respects the desired output order,
    /// so we can do a linear merge to form the output.
    fn build_collation<S>(
        &self,
        arrangements: Vec<(ReductionType, RowRowArrangement<S>)>,
        aggregate_types: Vec<ReductionType>,
        scope: &mut S,
    ) -> (RowRowArrangement<S>, Collection<S, DataflowError, Diff>)
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        let error_logger = self.error_logger();

        // We must have more than one arrangement to collate.
        if arrangements.len() <= 1 {
            error_logger.soft_panic_or_log(
                "Incorrect number of arrangements in reduce collation",
                &format!("len={}", arrangements.len()),
            );
        }

        let mut to_concat = vec![];

        // First, lets collect all results into a single collection.
        for (reduction_type, arrangement) in arrangements.into_iter() {
            let collection = arrangement.as_collection(move |key, val| {
                (key.into_owned(), (reduction_type, val.into_owned()))
            });
            to_concat.push(collection);
        }

        // For each key above, we need to have exactly as many rows as there are distinct
        // reduction types required by `aggregate_types`. We thus prepare here a properly
        // deduplicated version of `aggregate_types` for validation during processing below.
        let mut distinct_aggregate_types = aggregate_types.clone();
        distinct_aggregate_types.sort_unstable();
        distinct_aggregate_types.dedup();
        let n_distinct_aggregate_types = distinct_aggregate_types.len();

        let aggregate_types_err = aggregate_types.clone();
        use differential_dataflow::collection::concatenate;
        let (oks, errs) = concatenate(scope, to_concat)
            .mz_arrange::<RowValSpine<_, _, _>>("Arrange ReduceCollation")
            .reduce_pair::<_, RowRowSpine<_, _>, _, RowErrSpine<_, _>>(
                "ReduceCollation",
                "ReduceCollation Errors",
                {
                    move |_key, input, output| {
                        // The inputs are pairs of a reduction type, and a row consisting of densely
                        // packed fused aggregate values.
                        //
                        // We need to reconstitute the final value by:
                        // 1. Extracting out the fused rows by type
                        // 2. For each aggregate, figure out what type it is, and grab the relevant
                        //    value from the corresponding fused row.
                        // 3. Stitch all the values together into one row.
                        let mut accumulable = DatumList::empty().iter();
                        let mut hierarchical = DatumList::empty().iter();
                        let mut basic = DatumList::empty().iter();

                        // Note that hierarchical and basic reductions guard against negative
                        // multiplicities, and if we only had accumulable aggregations, we would not
                        // have produced a collation plan, so we do not repeat the check here.
                        if input.len() != n_distinct_aggregate_types {
                            return;
                        }

                        for (item, _) in input.iter() {
                            let reduction_type = &item.0;
                            let row = &item.1;
                            match reduction_type {
                                ReductionType::Accumulable => accumulable = row.iter(),
                                ReductionType::Hierarchical => hierarchical = row.iter(),
                                ReductionType::Basic => basic = row.iter(),
                            }
                        }

                        // Merge results into the order they were asked for.
                        let binding = SharedRow::get();
                        let mut row_builder = binding.borrow_mut();
                        let mut row_packer = row_builder.packer();
                        for typ in aggregate_types.iter() {
                            let datum = match typ {
                                ReductionType::Accumulable => accumulable.next(),
                                ReductionType::Hierarchical => hierarchical.next(),
                                ReductionType::Basic => basic.next(),
                            };
                            let Some(datum) = datum else { return };
                            row_packer.push(datum);
                        }
                        // If we did not have enough values to stitch together, then we do not generate
                        // an output row. Not outputting here corresponds to the semantics of an
                        // equi-join on the key, similarly to the proposal in PR #17013.
                        //
                        // Note that we also do not want to have anything left over to stich.  If we do,
                        // then we also have an error and would violate join semantics.
                        if (accumulable.next(), hierarchical.next(), basic.next())
                            == (None, None, None)
                        {
                            output.push((row_builder.clone(), 1));
                        }
                    }
                },
                move |key, input, output| {
                    if input.len() != n_distinct_aggregate_types {
                        // We expected to stitch together exactly as many aggregate types as requested
                        // by the collation. If we cannot, we log an error and produce no output for
                        // this key.
                        let message = "Mismatched aggregates for key in ReduceCollation";
                        error_logger.log(
                            message,
                            &format!(
                                "key={key:?}, n_aggregates_requested={requested}, \
                                 n_distinct_aggregate_types={n_distinct_aggregate_types}",
                                requested = input.len(),
                            ),
                        );
                        output.push((EvalError::Internal(message.to_string()).into(), 1));
                        return;
                    }

                    let mut accumulable = DatumList::empty().iter();
                    let mut hierarchical = DatumList::empty().iter();
                    let mut basic = DatumList::empty().iter();
                    for (item, _) in input.iter() {
                        let reduction_type = &item.0;
                        let row = &item.1;
                        match reduction_type {
                            ReductionType::Accumulable => accumulable = row.iter(),
                            ReductionType::Hierarchical => hierarchical = row.iter(),
                            ReductionType::Basic => basic = row.iter(),
                        }
                    }

                    for typ in aggregate_types_err.iter() {
                        let datum = match typ {
                            ReductionType::Accumulable => accumulable.next(),
                            ReductionType::Hierarchical => hierarchical.next(),
                            ReductionType::Basic => basic.next(),
                        };
                        if datum.is_some() {
                            continue;
                        }

                        // We cannot properly reconstruct a row if aggregates are missing.
                        // This situation is not expected, so we log an error if it occurs.
                        let message = "Missing value for key in ReduceCollation";
                        error_logger.log(message, &format!("typ={typ:?}, key={key:?}"));
                        output.push((EvalError::Internal(message.to_string()).into(), 1));
                        return;
                    }

                    // Note that we also do not want to have anything left over to stich.
                    // If we do, then we also have an error and would violate join semantics.
                    if (accumulable.next(), hierarchical.next(), basic.next()) == (None, None, None)
                    {
                        return;
                    }

                    let message = "Rows too large for key in ReduceCollation";
                    error_logger.log(message, &format!("key={key:?}"));
                    output.push((EvalError::Internal(message.to_string()).into(), 1));
                },
            );
        (oks, errs.as_collection(|_, v| v.into_owned()))
    }

    fn dispatch_build_distinct<S>(
        &self,
        collection: Collection<S, (Row, Row), Diff>,
    ) -> (
        SpecializedArrangement<S>,
        Collection<S, DataflowError, Diff>,
    )
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        if self.enable_specialized_arrangements {
            let collection = collection.map(|(k, v)| {
                assert!(v.is_empty());
                (k, ())
            });
            let (arrangement, errs) = self
                .build_distinct::<RowSpine<_, _>, RowValSpine<_, _, _>, _>(
                    collection,
                    " [val: empty]",
                );
            (SpecializedArrangement::RowUnit(arrangement), errs)
        } else {
            let collection = collection.inspect(|((_, v), _, _)| assert!(v.is_empty()));
            let (arrangement, errs) =
                self.build_distinct::<RowRowSpine<_, _>, RowValSpine<_, _, _>, _>(collection, "");
            (SpecializedArrangement::RowRow(arrangement), errs)
        }
    }

    /// Build the dataflow to compute the set of distinct keys.
    fn build_distinct<T1, T2, S>(
        &self,
        collection: Collection<S, (Row, T1::ValOwned), Diff>,
        tag: &str,
    ) -> (
        Arranged<S, TraceAgent<T1>>,
        Collection<S, DataflowError, Diff>,
    )
    where
        S: Scope<Timestamp = G::Timestamp>,
        T1: Trace<KeyOwned = Row, Time = G::Timestamp, Diff = Diff> + 'static,
        T1::Batch: Batch,
        T1::Batcher: Batcher<Item = ((Row, T1::ValOwned), G::Timestamp, Diff)>,
        T1::ValOwned: Columnation + ExchangeData + Default,
        Arranged<S, TraceAgent<T1>>: ArrangementSize,
        T2: for<'a> Trace<
                Key<'a> = T1::Key<'a>,
                KeyOwned = Row,
                ValOwned = DataflowError,
                Time = G::Timestamp,
                Diff = Diff,
            > + 'static,
        T2::Batch: Batch,
        T2::Batcher: Batcher<Item = ((Row, T2::ValOwned), G::Timestamp, Diff)>,
        Arranged<S, TraceAgent<T2>>: ArrangementSize,
    {
        let error_logger = self.error_logger();

        let (input_name, output_name) = (
            format!("Arranged DistinctBy{}", tag),
            format!("DistinctBy{}", tag),
        );

        let (output, errors) = collection
            .mz_arrange::<T1>(&input_name)
            .reduce_pair::<_, T1, _, T2>(
                &output_name,
                "DistinctByErrorCheck",
                |_key, _input, output| {
                    // We're pushing a unit value here because the key is implicitly added by the
                    // arrangement, and the permutation logic takes care of using the key part of the
                    // output.
                    output.push((T1::ValOwned::default(), 1));
                },
                move |key, input: &[(_, Diff)], output| {
                    for (_, count) in input.iter() {
                        if count.is_positive() {
                            continue;
                        }
                        let key = key.into_owned();
                        let message = "Non-positive multiplicity in DistinctBy";
                        error_logger.log(message, &format!("row={key:?}, count={count}"));
                        output.push((EvalError::Internal(message.to_string()).into(), 1));
                        return;
                    }
                },
            );
        (output, errors.as_collection(|_k, v| v.into_owned()))
    }

    /// Build the dataflow to compute and arrange multiple non-accumulable,
    /// non-hierarchical aggregations on `input`.
    ///
    /// This function assumes that we are explicitly rendering multiple basic aggregations.
    /// For each aggregate, we render a different reduce operator, and then fuse
    /// results together into a final arrangement that presents all the results
    /// in the order specified by `aggrs`.
    fn build_basic_aggregates<S>(
        &self,
        input: Collection<S, (Row, Row), Diff>,
        aggrs: Vec<(usize, AggregateExpr)>,
    ) -> (RowRowArrangement<S>, Collection<S, DataflowError, Diff>)
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        // We are only using this function to render multiple basic aggregates and
        // stitch them together. If that's not true we should complain.
        if aggrs.len() <= 1 {
            self.error_logger().soft_panic_or_log(
                "Too few aggregations when building basic aggregates",
                &format!("len={}", aggrs.len()),
            )
        }
        let mut err_output = None;
        let mut to_collect = Vec::new();
        for (index, aggr) in aggrs {
            let (result, errs) =
                self.build_basic_aggregate(input.clone(), index, &aggr, err_output.is_none());
            if errs.is_some() {
                err_output = errs
            }
            to_collect.push(
                result.as_collection(move |key, val| (key.into_owned(), (index, val.into_owned()))),
            );
        }
        let output = differential_dataflow::collection::concatenate(&mut input.scope(), to_collect)
            .mz_arrange::<RowValSpine<_, _, _>>("Arranged ReduceFuseBasic input")
            .mz_reduce_abelian::<_, RowRowSpine<_, _>>("ReduceFuseBasic", {
                move |_key, input, output| {
                    let binding = SharedRow::get();
                    let mut row_builder = binding.borrow_mut();
                    let mut row_packer = row_builder.packer();
                    for (item, _) in input.iter() {
                        let row = &item.1;
                        let datum = row.unpack_first();
                        row_packer.push(datum);
                    }
                    output.push((row_builder.clone(), 1));
                }
            });
        (
            output,
            err_output.expect("expected to validate in at least one aggregate"),
        )
    }

    /// Build the dataflow to compute a single basic aggregation.
    ///
    /// This method also applies distinctness if required.
    fn build_basic_aggregate<S>(
        &self,
        input: Collection<S, (Row, Row), Diff>,
        index: usize,
        aggr: &AggregateExpr,
        validating: bool,
    ) -> (
        RowRowArrangement<S>,
        Option<Collection<S, DataflowError, Diff>>,
    )
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        let AggregateExpr {
            func,
            expr: _,
            distinct,
        } = aggr.clone();

        // Extract the value we were asked to aggregate over.
        let mut partial = input.map(move |(key, row)| {
            let binding = SharedRow::get();
            let mut row_builder = binding.borrow_mut();
            let value = row.iter().nth(index).unwrap();
            row_builder.packer().push(value);
            (key, row_builder.clone())
        });

        let mut err_output = None;

        // If `distinct` is set, we restrict ourselves to the distinct `(key, val)`.
        if distinct {
            if validating {
                let (oks, errs) = self
                    .build_reduce_inaccumulable_distinct::<_, KeyValSpine<_, Result<(), String>, _, _>>(partial, None)
                    .as_collection(|k, v| (k.into_owned(), v.into_owned()))
                    .map_fallible("Demux Errors", move |(key, result)| match result {
                        Ok(()) => Ok(key),
                        Err(m) => Err(EvalError::Internal(m).into()),
                    });
                err_output = Some(errs);
                partial = oks;
            } else {
                partial = self
                    .build_reduce_inaccumulable_distinct::<_, KeySpine<_, _, _>>(
                        partial,
                        Some(" [val: empty]"),
                    )
                    .as_collection(|k, _| k.into_owned());
            }
        }

        let arranged = partial.mz_arrange::<RowRowSpine<_, _>>("Arranged ReduceInaccumulable");
        let oks = arranged.mz_reduce_abelian::<_, RowRowSpine<_, _>>("ReduceInaccumulable", {
            move |_key, source, target| {
                // We respect the multiplicity here (unlike in hierarchical aggregation)
                // because we don't know that the aggregation method is not sensitive
                // to the number of records.
                let iter = source.iter().flat_map(|(v, w)| {
                    // Note that in the non-positive case, this is wrong, but harmless because
                    // our other reduction will produce an error.
                    let count = usize::try_from(*w).unwrap_or(0);
                    use mz_repr::fixed_length::IntoRowByTypes;
                    std::iter::repeat(v.into_datum_iter(None).next().unwrap()).take(count)
                });
                let binding = SharedRow::get();
                let mut row_builder = binding.borrow_mut();
                row_builder.packer().push(
                    // Note that this is not necessarily a window aggregation, in which case
                    // `eval_fast_window_agg` delegates to the normal `eval`.
                    func.eval_fast_window_agg::<_, window_agg_helpers::OneByOneAggrImpls>(
                        iter,
                        &RowArena::new(),
                    ),
                );
                target.push((row_builder.clone(), 1));
            }
        });

        // Note that we would prefer to use `mz_timely_util::reduce::ReduceExt::reduce_pair` here, but
        // we then wouldn't be able to do this error check conditionally.  See its documentation for the
        // rationale around using a second reduction here.
        if validating && err_output.is_none() {
            let error_logger = self.error_logger();

            let errs = arranged.mz_reduce_abelian::<_, RowErrSpine<_, _>>(
                "ReduceInaccumulable Error Check",
                move |_key, source, target| {
                    // Negative counts would be surprising, but until we are 100% certain we won't
                    // see them, we should report when we do. We may want to bake even more info
                    // in here in the future.
                    for (value, count) in source.iter() {
                        if count.is_positive() {
                            continue;
                        }
                        let value = value.into_owned();
                        let message = "Non-positive accumulation in ReduceInaccumulable";
                        error_logger.log(message, &format!("value={value:?}, count={count}"));
                        target.push((EvalError::Internal(message.to_string()).into(), 1));
                        return;
                    }
                },
            );
            (oks, Some(errs.as_collection(|_, v| v.into_owned())))
        } else {
            (oks, err_output)
        }
    }

    fn build_reduce_inaccumulable_distinct<S, Tr>(
        &self,
        input: Collection<S, (Row, Row), Diff>,
        name_tag: Option<&str>,
    ) -> Arranged<S, TraceAgent<Tr>>
    where
        S: Scope<Timestamp = G::Timestamp>,
        Tr::ValOwned: MaybeValidatingRow<(), String>,
        Tr: Trace
            + for<'a> TraceReader<
                Key<'a> = &'a (Row, Row),
                KeyOwned = (Row, Row),
                Time = G::Timestamp,
                Diff = Diff,
            > + 'static,
        Tr::Batch: Batch,
        Tr::Batcher: Batcher<Item = (((Row, Row), Tr::ValOwned), G::Timestamp, Diff)>,
        Arranged<S, TraceAgent<Tr>>: ArrangementSize,
    {
        let error_logger = self.error_logger();

        let output_name = format!(
            "ReduceInaccumulable Distinct{}",
            name_tag.unwrap_or_default()
        );

        let input: KeyCollection<_, _, _> = input.into();
        input
            .mz_arrange::<KeySpine<(Row, Row), _, _>>(
                "Arranged ReduceInaccumulable Distinct [val: empty]",
            )
            .mz_reduce_abelian::<_, Tr>(&output_name, move |_, source, t| {
                if let Some(err) = Tr::ValOwned::into_error() {
                    for (value, count) in source.iter() {
                        if count.is_positive() {
                            continue;
                        }

                        let message = "Non-positive accumulation in ReduceInaccumulable DISTINCT";
                        error_logger.log(message, &format!("value={value:?}, count={count}"));
                        t.push((err(message.to_string()), 1));
                        return;
                    }
                }
                t.push((Tr::ValOwned::ok(()), 1))
            })
    }

    /// Build the dataflow to compute and arrange multiple hierarchical aggregations
    /// on non-monotonic inputs.
    ///
    /// This function renders a single reduction tree that computes aggregations with
    /// a priority queue implemented with a series of reduce operators that partition
    /// the input into buckets, and compute the aggregation over very small buckets
    /// and feed the results up to larger buckets.
    ///
    /// Note that this implementation currently ignores the distinct bit because we
    /// currently only perform min / max hierarchically and the reduction tree
    /// efficiently suppresses non-distinct updates.
    fn build_bucketed<S>(
        &self,
        input: Collection<S, (Row, Row), Diff>,
        BucketedPlan {
            aggr_funcs,
            skips,
            buckets,
        }: BucketedPlan,
    ) -> (
        RowRowArrangement<S>,
        Option<Collection<S, DataflowError, Diff>>,
    )
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        let mut err_output: Option<Collection<S, _, _>> = None;
        let arranged_output = input.scope().region_named("ReduceHierarchical", |inner| {
            let input = input.enter(inner);

            // Gather the relevant values into a vec of rows ordered by aggregation_index
            let input = input.map(move |(key, row)| {
                let binding = SharedRow::get();
                let mut row_builder = binding.borrow_mut();
                let mut values = Vec::with_capacity(skips.len());
                let mut row_iter = row.iter();
                for skip in skips.iter() {
                    row_builder.packer().push(row_iter.nth(*skip).unwrap());
                    values.push(row_builder.clone());
                }

                (key, values)
            });

            // Repeatedly apply hierarchical reduction with a progressively coarser key.
            let mut stage = input.map(move |(key, values)| ((key, values.hashed()), values));
            let mut validating = true;
            for b in buckets.into_iter() {
                let input = stage.map(move |((key, hash), values)| ((key, hash % b), values));

                // We only want the first stage to perform validation of whether invalid accumulations
                // were observed in the input. Subsequently, we will either produce an error in the error
                // stream or produce correct data in the output stream.
                let negated_output = if validating {
                    let (oks, errs) = self
                        .build_bucketed_negated_output::<_, Result<Vec<Row>, (Row, u64)>>(
                            &input,
                            aggr_funcs.clone(),
                        )
                        .map_fallible(
                            "Checked Invalid Accumulations",
                            |(key, result)| match result {
                                Err((key, _)) => {
                                    let message = format!(
                                        "Invalid data in source, saw non-positive accumulation \
                                         for key {key:?} in hierarchical mins-maxes aggregate"
                                    );
                                    Err(EvalError::Internal(message).into())
                                }
                                Ok(values) => Ok((key, values)),
                            },
                        );
                    validating = false;
                    err_output = Some(errs.leave_region());
                    oks
                } else {
                    self.build_bucketed_negated_output::<_, Vec<Row>>(&input, aggr_funcs.clone())
                };

                stage = negated_output
                    .negate()
                    .concat(&input)
                    .consolidate_named::<KeySpine<_, _, _>>("Consolidated MinsMaxesHierarchical");
            }

            // Discard the hash from the key and return to the format of the input data.
            let partial = stage.map(|((key, _hash), values)| (key, values));

            // Build a series of stages for the reduction
            // Arrange the final result into (key, Row)
            let error_logger = self.error_logger();
            // NOTE(vmarcos): The input operator name below is used in the tuning advice built-in
            // view mz_internal.mz_expected_group_size_advice.
            let arranged =
                partial.mz_arrange::<RowValSpine<Vec<Row>, _, _>>("Arrange ReduceMinsMaxes");
            // Note that we would prefer to use `mz_timely_util::reduce::ReduceExt::reduce_pair` here,
            // but we then wouldn't be able to do this error check conditionally.  See its documentation
            // for the rationale around using a second reduction here.
            if validating {
                let errs = arranged
                    .mz_reduce_abelian::<_, RowErrSpine<_, _>>(
                        "ReduceMinsMaxes Error Check",
                        move |_key, source, target| {
                            // Negative counts would be surprising, but until we are 100% certain we wont
                            // see them, we should report when we do. We may want to bake even more info
                            // in here in the future.
                            for (val, count) in source.iter() {
                                if count.is_positive() {
                                    continue;
                                }
                                let val = val.into_owned();
                                let message = "Non-positive accumulation in ReduceMinsMaxes";
                                error_logger.log(message, &format!("val={val:?}, count={count}"));
                                target.push((EvalError::Internal(message.to_string()).into(), 1));
                                return;
                            }
                        },
                    )
                    .as_collection(|_, v| v.into_owned());
                err_output = Some(errs.leave_region());
            }
            arranged
                .mz_reduce_abelian::<_, RowRowSpine<_, _>>("ReduceMinsMaxes", {
                    move |_key, source, target| {
                        let binding = SharedRow::get();
                        let mut row_builder = binding.borrow_mut();
                        let mut row_packer = row_builder.packer();
                        for (aggr_index, func) in aggr_funcs.iter().enumerate() {
                            let iter = source
                                .iter()
                                .map(|(values, _cnt)| values[aggr_index].iter().next().unwrap());
                            row_packer.push(func.eval(iter, &RowArena::new()));
                        }
                        target.push((row_builder.clone(), 1));
                    }
                })
                .leave_region()
        });
        (arranged_output, err_output)
    }

    /// Build the dataflow for one stage of a reduction tree for multiple hierarchical
    /// aggregates.
    ///
    /// `buckets` indicates the number of buckets in this stage. We do some non
    /// obvious trickery here to limit the memory usage per layer by internally
    /// holding only the elements that were rejected by this stage. However, the
    /// output collection maintains the `((key, bucket), (passing value)` for this
    /// stage.
    /// `validating` indicates whether we want this stage to perform error detection
    /// for invalid accumulations. Once a stage is clean of such errors, subsequent
    /// stages can skip validation.
    fn build_bucketed_negated_output<S, R>(
        &self,
        input: &Collection<S, ((Row, u64), Vec<Row>), Diff>,
        aggrs: Vec<AggregateFunc>,
    ) -> Collection<S, ((Row, u64), R), Diff>
    where
        S: Scope<Timestamp = G::Timestamp>,
        R: MaybeValidatingRow<Vec<Row>, (Row, u64)>,
    {
        let error_logger = self.error_logger();
        // NOTE(vmarcos): The input operator name below is used in the tuning advice built-in
        // view mz_internal.mz_expected_group_size_advice.
        let arranged_input = input
            .mz_arrange::<KeyValSpine<_, Vec<Row>, _, _>>("Arranged MinsMaxesHierarchical input");

        arranged_input
            .mz_reduce_abelian::<_, KeyValSpine<_, _, _, _>>(
                "Reduced Fallibly MinsMaxesHierarchical",
                move |key, source, target| {
                    if let Some(err) = R::into_error() {
                        // Should negative accumulations reach us, we should loudly complain.
                        for (value, count) in source.iter() {
                            if count.is_positive() {
                                continue;
                            }
                            error_logger.log(
                                "Non-positive accumulation in MinsMaxesHierarchical",
                                &format!("key={key:?}, value={value:?}, count={count}"),
                            );
                            // After complaining, output an error here so that we can eventually
                            // report it in an error stream.
                            target.push((err(key.clone()), -1));
                            return;
                        }
                    }
                    let binding = SharedRow::get();
                    let mut row_builder = binding.borrow_mut();
                    let mut output = Vec::with_capacity(aggrs.len());
                    for (aggr_index, func) in aggrs.iter().enumerate() {
                        let iter = source
                            .iter()
                            .map(|(values, _cnt)| values[aggr_index].iter().next().unwrap());
                        row_builder.packer().push(func.eval(iter, &RowArena::new()));
                        output.push(row_builder.clone());
                    }
                    // We only want to arrange the parts of the input that are not part of the output.
                    // More specifically, we want to arrange it so that `input.concat(&output.negate())`
                    // gives us the intended value of this aggregate function. Also we assume that regardless
                    // of the multiplicity of the final result in the input, we only want to have one copy
                    // in the output.
                    target.push((R::ok(output), -1));
                    target.extend(
                        source
                            .iter()
                            .map(|(values, cnt)| (R::ok((*values).clone()), *cnt)),
                    );
                },
            )
            .as_collection(|k, v| (k.clone(), v.clone()))
    }

    /// Build the dataflow to compute and arrange multiple hierarchical aggregations
    /// on monotonic inputs.
    fn build_monotonic<S>(
        &self,
        collection: Collection<S, (Row, Row), Diff>,
        MonotonicPlan {
            aggr_funcs,
            skips,
            must_consolidate,
        }: MonotonicPlan,
    ) -> (RowRowArrangement<S>, Collection<S, DataflowError, Diff>)
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        // Gather the relevant values into a vec of rows ordered by aggregation_index
        let collection = collection
            .map(move |(key, row)| {
                let binding = SharedRow::get();
                let mut row_builder = binding.borrow_mut();
                let mut values = Vec::with_capacity(skips.len());
                let mut row_iter = row.iter();
                for skip in skips.iter() {
                    row_builder.packer().push(row_iter.nth(*skip).unwrap());
                    values.push(row_builder.clone());
                }

                (key, values)
            })
            .consolidate_named_if::<KeySpine<_, _, _>>(
                must_consolidate,
                "Consolidated ReduceMonotonic input",
            );

        // It should be now possible to ensure that we have a monotonic collection.
        let error_logger = self.error_logger();
        let (partial, errs) = collection.ensure_monotonic(move |data, diff| {
            error_logger.log(
                "Non-monotonic input to ReduceMonotonic",
                &format!("data={data:?}, diff={diff}"),
            );
            let m = "tried to build a monotonic reduction on non-monotonic input".to_string();
            (EvalError::Internal(m).into(), 1)
        });
        // We can place our rows directly into the diff field, and
        // only keep the relevant one corresponding to evaluating our
        // aggregate, instead of having to do a hierarchical reduction.
        let partial = partial.explode_one(move |(key, values)| {
            let mut output = Vec::new();
            for (row, func) in values.into_iter().zip(aggr_funcs.iter()) {
                output.push(monoids::get_monoid(row, func).expect(
                    "hierarchical aggregations are expected to have monoid implementations",
                ));
            }
            (key, output)
        });
        let partial: KeyCollection<_, _, _> = partial.into();
        let output = partial
            .mz_arrange::<RowSpine<_, Vec<ReductionMonoid>>>("ArrangeMonotonic [val: empty]")
            .mz_reduce_abelian::<_, RowRowSpine<_, _>>("ReduceMonotonic", {
                move |_key, input, output| {
                    let binding = SharedRow::get();
                    let mut row_builder = binding.borrow_mut();
                    let mut row_packer = row_builder.packer();
                    let accum = &input[0].1;
                    for monoid in accum.iter() {
                        row_packer.extend(monoid.finalize().iter());
                    }
                    output.push((row_builder.clone(), 1));
                }
            });
        (output, errs)
    }

    /// Build the dataflow to compute and arrange multiple accumulable aggregations.
    ///
    /// The incoming values are moved to the update's "difference" field, at which point
    /// they can be accumulated in place. The `count` operator promotes the accumulated
    /// values to data, at which point a final map applies operator-specific logic to
    /// yield the final aggregate.
    fn build_accumulable<S>(
        &self,
        collection: Collection<S, (Row, Row), Diff>,
        AccumulablePlan {
            full_aggrs,
            simple_aggrs,
            distinct_aggrs,
        }: AccumulablePlan,
    ) -> (RowRowArrangement<S>, Collection<S, DataflowError, Diff>)
    where
        S: Scope<Timestamp = G::Timestamp>,
    {
        // we must have called this function with something to reduce
        if full_aggrs.len() == 0 || simple_aggrs.len() + distinct_aggrs.len() != full_aggrs.len() {
            self.error_logger().soft_panic_or_log(
                "Incorrect numbers of aggregates in accummulable reduction rendering",
                &format!(
                    "full_aggrs={}, simple_aggrs={}, distinct_aggrs={}",
                    full_aggrs.len(),
                    simple_aggrs.len(),
                    distinct_aggrs.len(),
                ),
            );
        }

        // Some of the aggregations may have the `distinct` bit set, which means that they'll
        // need to be extracted from `collection` and be subjected to `distinct` with `key`.
        // Other aggregations can be directly moved in to the `diff` field.
        //
        // In each case, the resulting collection should have `data` shaped as `(key, ())`
        // and a `diff` that is a vector with length `3 * aggrs.len()`. The three values are
        // generally the count, and then two aggregation-specific values. The size could be
        // reduced if we want to specialize for the aggregations.

        // Instantiate a default vector for diffs with the correct types at each
        // position.
        let zero_diffs: (Vec<_>, Diff) = (
            full_aggrs
                .iter()
                .map(|f| accumulable_zero(&f.func))
                .collect(),
            0,
        );

        let mut to_aggregate = Vec::new();
        if simple_aggrs.len() > 0 {
            // First, collect all non-distinct aggregations in one pass.
            let easy_cases = collection.explode_one({
                let zero_diffs = zero_diffs.clone();
                move |(key, row)| {
                    let mut diffs = zero_diffs.clone();
                    // Try to unpack only the datums we need. Unfortunately, since we
                    // can't random access into a Row, we have to iterate through one by one.
                    // TODO: Even though we don't have random access, we could still avoid unpacking
                    // everything that we don't care about, and it might be worth it to extend the
                    // Row API to do that.
                    let mut row_iter = row.iter().enumerate();
                    for (accumulable_index, datum_index, aggr) in simple_aggrs.iter() {
                        let mut datum = row_iter.next().unwrap();
                        while datum_index != &datum.0 {
                            datum = row_iter.next().unwrap();
                        }
                        let datum = datum.1;
                        diffs.0[*accumulable_index] = datum_to_accumulator(&aggr.func, datum);
                        diffs.1 = 1;
                    }
                    ((key, ()), diffs)
                }
            });
            to_aggregate.push(easy_cases);
        }

        // Next, collect all aggregations that require distinctness.
        for (accumulable_index, datum_index, aggr) in distinct_aggrs.into_iter() {
            let collection = collection
                .map(move |(key, row)| {
                    let binding = SharedRow::get();
                    let mut row_builder = binding.borrow_mut();
                    let value = row.iter().nth(datum_index).unwrap();
                    row_builder.packer().push(value);
                    (key, row_builder.clone())
                })
                .map(|k| (k, ()))
                .mz_arrange::<KeySpine<(Row, Row), _, _>>(
                    "Arranged Accumulable Distinct [val: empty]",
                )
                .mz_reduce_abelian::<_, KeySpine<_, _, _>>(
                    "Reduced Accumulable Distinct [val: empty]",
                    move |_k, _s, t| t.push(((), 1)),
                )
                .as_collection(|k, _| k.clone())
                .explode_one({
                    let zero_diffs = zero_diffs.clone();
                    move |(key, row)| {
                        let datum = row.iter().next().unwrap();
                        let mut diffs = zero_diffs.clone();
                        diffs.0[accumulable_index] = datum_to_accumulator(&aggr.func, datum);
                        diffs.1 = 1;
                        ((key, ()), diffs)
                    }
                });
            to_aggregate.push(collection);
        }

        // now concatenate, if necessary, multiple aggregations
        let collection = if to_aggregate.len() == 1 {
            to_aggregate.remove(0)
        } else {
            differential_dataflow::collection::concatenate(&mut collection.scope(), to_aggregate)
        };

        let error_logger = self.error_logger();
        let err_full_aggrs = full_aggrs.clone();
        let (arranged_output, arranged_errs) = collection
            .mz_arrange::<RowSpine<_, (Vec<Accum>, Diff)>>("ArrangeAccumulable [val: empty]")
            .reduce_pair::<_, RowRowSpine<_, _>, _, RowErrSpine<_, _>>(
                "ReduceAccumulable",
                "AccumulableErrorCheck",
                {
                    move |_key, input, output| {
                        let binding = SharedRow::get();
                        let mut row_builder = binding.borrow_mut();
                        let (ref accums, total) = input[0].1;
                        let mut row_packer = row_builder.packer();

                        for (aggr, accum) in full_aggrs.iter().zip(accums) {
                            row_packer.push(finalize_accum(&aggr.func, accum, total));
                        }
                        output.push((row_builder.clone(), 1));
                    }
                },
                move |key, input, output| {
                    let (ref accums, total) = input[0].1;
                    for (aggr, accum) in err_full_aggrs.iter().zip(accums) {
                        // We first test here if inputs without net-positive records are present,
                        // producing an error to the logs and to the query output if that is the case.
                        if total == 0 && !accum.is_zero() {
                            error_logger.log(
                                "Net-zero records with non-zero accumulation in ReduceAccumulable",
                                &format!("aggr={aggr:?}, accum={accum:?}"),
                            );
                            let message = format!(
                                "Invalid data in source, saw net-zero records for key {key} \
                                 with non-zero accumulation in accumulable aggregate"
                            );
                            output.push((EvalError::Internal(message).into(), 1));
                        }
                        match (&aggr.func, &accum) {
                            (AggregateFunc::SumUInt16, Accum::SimpleNumber { accum, .. })
                            | (AggregateFunc::SumUInt32, Accum::SimpleNumber { accum, .. })
                            | (AggregateFunc::SumUInt64, Accum::SimpleNumber { accum, .. }) => {
                                if accum.is_negative() {
                                    error_logger.log(
                                    "Invalid negative unsigned aggregation in ReduceAccumulable",
                                    &format!("aggr={aggr:?}, accum={accum:?}"),
                                );
                                    let message = format!(
                                        "Invalid data in source, saw negative accumulation with \
                                         unsigned type for key {key}"
                                    );
                                    output.push((EvalError::Internal(message).into(), 1));
                                }
                            }
                            _ => (), // no more errors to check for at this point!
                        }
                    }
                },
            );
        (
            arranged_output,
            arranged_errs.as_collection(|_key, error| error.into_owned()),
        )
    }
}

fn accumulable_zero(aggr_func: &AggregateFunc) -> Accum {
    match aggr_func {
        AggregateFunc::Any | AggregateFunc::All => Accum::Bool {
            trues: 0,
            falses: 0,
        },
        AggregateFunc::SumFloat32 | AggregateFunc::SumFloat64 => Accum::Float {
            accum: 0,
            pos_infs: 0,
            neg_infs: 0,
            nans: 0,
            non_nulls: 0,
        },
        AggregateFunc::SumNumeric => Accum::Numeric {
            accum: OrderedDecimal(NumericAgg::zero()),
            pos_infs: 0,
            neg_infs: 0,
            nans: 0,
            non_nulls: 0,
        },
        _ => Accum::SimpleNumber {
            accum: 0,
            non_nulls: 0,
        },
    }
}

static FLOAT_SCALE: Lazy<f64> = Lazy::new(|| f64::from(1 << 24));

fn datum_to_accumulator(aggregate_func: &AggregateFunc, datum: Datum) -> Accum {
    match aggregate_func {
        AggregateFunc::Count => Accum::SimpleNumber {
            accum: 0, // unused for AggregateFunc::Count
            non_nulls: if datum.is_null() { 0 } else { 1 },
        },
        AggregateFunc::Any | AggregateFunc::All => match datum {
            Datum::True => Accum::Bool {
                trues: 1,
                falses: 0,
            },
            Datum::Null => Accum::Bool {
                trues: 0,
                falses: 0,
            },
            Datum::False => Accum::Bool {
                trues: 0,
                falses: 1,
            },
            x => panic!("Invalid argument to AggregateFunc::Any: {x:?}"),
        },
        AggregateFunc::Dummy => match datum {
            Datum::Dummy => Accum::SimpleNumber {
                accum: 0,
                non_nulls: 0,
            },
            x => panic!("Invalid argument to AggregateFunc::Dummy: {x:?}"),
        },
        AggregateFunc::SumFloat32 | AggregateFunc::SumFloat64 => {
            let n = match datum {
                Datum::Float32(n) => f64::from(*n),
                Datum::Float64(n) => *n,
                Datum::Null => 0f64,
                x => panic!("Invalid argument to AggregateFunc::{aggregate_func:?}: {x:?}"),
            };

            let nans = Diff::from(n.is_nan());
            let pos_infs = Diff::from(n == f64::INFINITY);
            let neg_infs = Diff::from(n == f64::NEG_INFINITY);
            let non_nulls = Diff::from(datum != Datum::Null);

            // Map the floating point value onto a fixed precision domain
            // All special values should map to zero, since they are tracked separately
            let accum = if nans > 0 || pos_infs > 0 || neg_infs > 0 {
                0
            } else {
                // This operation will truncate to i128::MAX if out of range.
                // TODO(benesch): rewrite to avoid `as`.
                #[allow(clippy::as_conversions)]
                {
                    (n * *FLOAT_SCALE) as i128
                }
            };

            Accum::Float {
                accum,
                pos_infs,
                neg_infs,
                nans,
                non_nulls,
            }
        }
        AggregateFunc::SumNumeric => match datum {
            Datum::Numeric(n) => {
                let (accum, pos_infs, neg_infs, nans) = if n.0.is_infinite() {
                    if n.0.is_negative() {
                        (NumericAgg::zero(), 0, 1, 0)
                    } else {
                        (NumericAgg::zero(), 1, 0, 0)
                    }
                } else if n.0.is_nan() {
                    (NumericAgg::zero(), 0, 0, 1)
                } else {
                    // Take a narrow decimal (datum) into a wide decimal
                    // (aggregator).
                    let mut cx_agg = numeric::cx_agg();
                    (cx_agg.to_width(n.0), 0, 0, 0)
                };

                Accum::Numeric {
                    accum: OrderedDecimal(accum),
                    pos_infs,
                    neg_infs,
                    nans,
                    non_nulls: 1,
                }
            }
            Datum::Null => Accum::Numeric {
                accum: OrderedDecimal(NumericAgg::zero()),
                pos_infs: 0,
                neg_infs: 0,
                nans: 0,
                non_nulls: 0,
            },
            x => panic!("Invalid argument to AggregateFunc::SumNumeric: {x:?}"),
        },
        _ => {
            // Other accumulations need to disentangle the accumulable
            // value from its NULL-ness, which is not quite as easily
            // accumulated.
            match datum {
                Datum::Int16(i) => Accum::SimpleNumber {
                    accum: i128::from(i),
                    non_nulls: 1,
                },
                Datum::Int32(i) => Accum::SimpleNumber {
                    accum: i128::from(i),
                    non_nulls: 1,
                },
                Datum::Int64(i) => Accum::SimpleNumber {
                    accum: i128::from(i),
                    non_nulls: 1,
                },
                Datum::UInt16(u) => Accum::SimpleNumber {
                    accum: i128::from(u),
                    non_nulls: 1,
                },
                Datum::UInt32(u) => Accum::SimpleNumber {
                    accum: i128::from(u),
                    non_nulls: 1,
                },
                Datum::UInt64(u) => Accum::SimpleNumber {
                    accum: i128::from(u),
                    non_nulls: 1,
                },
                Datum::MzTimestamp(t) => Accum::SimpleNumber {
                    accum: i128::from(u64::from(t)),
                    non_nulls: 1,
                },
                Datum::Null => Accum::SimpleNumber {
                    accum: 0,
                    non_nulls: 0,
                },
                x => panic!("Accumulating non-integer data: {x:?}"),
            }
        }
    }
}

fn finalize_accum<'a>(aggr_func: &'a AggregateFunc, accum: &'a Accum, total: Diff) -> Datum<'a> {
    // The finished value depends on the aggregation function in a variety of ways.
    // For all aggregates but count, if only null values were
    // accumulated, then the output is null.
    if total > 0 && accum.is_zero() && *aggr_func != AggregateFunc::Count {
        Datum::Null
    } else {
        match (&aggr_func, &accum) {
            (AggregateFunc::Count, Accum::SimpleNumber { non_nulls, .. }) => {
                Datum::Int64(*non_nulls)
            }
            (AggregateFunc::All, Accum::Bool { falses, trues }) => {
                // If any false, else if all true, else must be no false and some nulls.
                if *falses > 0 {
                    Datum::False
                } else if *trues == total {
                    Datum::True
                } else {
                    Datum::Null
                }
            }
            (AggregateFunc::Any, Accum::Bool { falses, trues }) => {
                // If any true, else if all false, else must be no true and some nulls.
                if *trues > 0 {
                    Datum::True
                } else if *falses == total {
                    Datum::False
                } else {
                    Datum::Null
                }
            }
            (AggregateFunc::Dummy, _) => Datum::Dummy,
            // If any non-nulls, just report the aggregate.
            (AggregateFunc::SumInt16, Accum::SimpleNumber { accum, .. })
            | (AggregateFunc::SumInt32, Accum::SimpleNumber { accum, .. }) => {
                // This conversion is safe, as long as we have less than 2^32
                // summands.
                // TODO(benesch): are we guaranteed to have less than 2^32 summands?
                // If so, rewrite to avoid `as`.
                #[allow(clippy::as_conversions)]
                Datum::Int64(*accum as i64)
            }
            (AggregateFunc::SumInt64, Accum::SimpleNumber { accum, .. }) => Datum::from(*accum),
            (AggregateFunc::SumUInt16, Accum::SimpleNumber { accum, .. })
            | (AggregateFunc::SumUInt32, Accum::SimpleNumber { accum, .. }) => {
                if !accum.is_negative() {
                    // Our semantics of overflow are not clearly articulated wrt.
                    // unsigned vs. signed types (#17758). We adopt an unsigned
                    // wrapping behavior to match what we do above for signed types.
                    // TODO(vmarcos): remove potentially dangerous usage of `as`.
                    #[allow(clippy::as_conversions)]
                    Datum::UInt64(*accum as u64)
                } else {
                    // Note that we return a value here, but an error in the other
                    // operator of the reduce_pair. Therefore, we expect that this
                    // value will never be exposed as an output.
                    Datum::Null
                }
            }
            (AggregateFunc::SumUInt64, Accum::SimpleNumber { accum, .. }) => {
                if !accum.is_negative() {
                    Datum::from(*accum)
                } else {
                    // Note that we return a value here, but an error in the other
                    // operator of the reduce_pair. Therefore, we expect that this
                    // value will never be exposed as an output.
                    Datum::Null
                }
            }
            (
                AggregateFunc::SumFloat32,
                Accum::Float {
                    accum,
                    pos_infs,
                    neg_infs,
                    nans,
                    non_nulls: _,
                },
            ) => {
                if *nans > 0 || (*pos_infs > 0 && *neg_infs > 0) {
                    // NaNs are NaNs and cases where we've seen a
                    // mixture of positive and negative infinities.
                    Datum::from(f32::NAN)
                } else if *pos_infs > 0 {
                    Datum::from(f32::INFINITY)
                } else if *neg_infs > 0 {
                    Datum::from(f32::NEG_INFINITY)
                } else {
                    // TODO(benesch): remove potentially dangerous usage of `as`.
                    #[allow(clippy::as_conversions)]
                    {
                        Datum::from(((*accum as f64) / *FLOAT_SCALE) as f32)
                    }
                }
            }
            (
                AggregateFunc::SumFloat64,
                Accum::Float {
                    accum,
                    pos_infs,
                    neg_infs,
                    nans,
                    non_nulls: _,
                },
            ) => {
                if *nans > 0 || (*pos_infs > 0 && *neg_infs > 0) {
                    // NaNs are NaNs and cases where we've seen a
                    // mixture of positive and negative infinities.
                    Datum::from(f64::NAN)
                } else if *pos_infs > 0 {
                    Datum::from(f64::INFINITY)
                } else if *neg_infs > 0 {
                    Datum::from(f64::NEG_INFINITY)
                } else {
                    // TODO(benesch): remove potentially dangerous usage of `as`.
                    #[allow(clippy::as_conversions)]
                    {
                        Datum::from((*accum as f64) / *FLOAT_SCALE)
                    }
                }
            }
            (
                AggregateFunc::SumNumeric,
                Accum::Numeric {
                    accum,
                    pos_infs,
                    neg_infs,
                    nans,
                    non_nulls: _,
                },
            ) => {
                let mut cx_datum = numeric::cx_datum();
                let d = cx_datum.to_width(accum.0);
                // Take a wide decimal (aggregator) into a
                // narrow decimal (datum). If this operation
                // overflows the datum, this new value will be
                // +/- infinity. However, the aggregator tracks
                // the amount of overflow, making it invertible.
                let inf_d = d.is_infinite();
                let neg_d = d.is_negative();
                let pos_inf = *pos_infs > 0 || (inf_d && !neg_d);
                let neg_inf = *neg_infs > 0 || (inf_d && neg_d);
                if *nans > 0 || (pos_inf && neg_inf) {
                    // NaNs are NaNs and cases where we've seen a
                    // mixture of positive and negative infinities.
                    Datum::from(Numeric::nan())
                } else if pos_inf {
                    Datum::from(Numeric::infinity())
                } else if neg_inf {
                    let mut cx = numeric::cx_datum();
                    let mut d = Numeric::infinity();
                    cx.neg(&mut d);
                    Datum::from(d)
                } else {
                    Datum::from(d)
                }
            }
            _ => panic!(
                "Unexpected accumulation (aggr={:?}, accum={accum:?})",
                aggr_func
            ),
        }
    }
}

/// Accumulates values for the various types of accumulable aggregations.
///
/// We assume that there are not more than 2^32 elements for the aggregation.
/// Thus we can perform a summation over i32 in an i64 accumulator
/// and not worry about exceeding its bounds.
///
/// The float accumulator performs accumulation in fixed point arithmetic. The fixed
/// point representation has less precision than a double. It is entirely possible
/// that the values of the accumulator overflow, thus we have to use wrapping arithmetic
/// to preserve group guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum Accum {
    /// Accumulates boolean values.
    Bool {
        /// The number of `true` values observed.
        trues: Diff,
        /// The number of `false` values observed.
        falses: Diff,
    },
    /// Accumulates simple numeric values.
    SimpleNumber {
        /// The accumulation of all non-NULL values observed.
        accum: i128,
        /// The number of non-NULL values observed.
        non_nulls: Diff,
    },
    /// Accumulates float values.
    Float {
        /// Accumulates non-special float values, mapped to a fixed precision i128 domain to
        /// preserve associativity and commutativity
        accum: i128,
        /// Counts +inf
        pos_infs: Diff,
        /// Counts -inf
        neg_infs: Diff,
        /// Counts NaNs
        nans: Diff,
        /// Counts non-NULL values
        non_nulls: Diff,
    },
    /// Accumulates arbitrary precision decimals.
    Numeric {
        /// Accumulates non-special values
        accum: OrderedDecimal<NumericAgg>,
        /// Counts +inf
        pos_infs: Diff,
        /// Counts -inf
        neg_infs: Diff,
        /// Counts NaNs
        nans: Diff,
        /// Counts non-NULL values
        non_nulls: Diff,
    },
}

impl Semigroup for Accum {
    fn is_zero(&self) -> bool {
        match self {
            Accum::Bool { trues, falses } => trues.is_zero() && falses.is_zero(),
            Accum::SimpleNumber { accum, non_nulls } => accum.is_zero() && non_nulls.is_zero(),
            Accum::Float {
                accum,
                pos_infs,
                neg_infs,
                nans,
                non_nulls,
            } => {
                accum.is_zero()
                    && pos_infs.is_zero()
                    && neg_infs.is_zero()
                    && nans.is_zero()
                    && non_nulls.is_zero()
            }
            Accum::Numeric {
                accum,
                pos_infs,
                neg_infs,
                nans,
                non_nulls,
            } => {
                accum.0.is_zero()
                    && pos_infs.is_zero()
                    && neg_infs.is_zero()
                    && nans.is_zero()
                    && non_nulls.is_zero()
            }
        }
    }

    fn plus_equals(&mut self, other: &Accum) {
        match (&mut *self, other) {
            (
                Accum::Bool { trues, falses },
                Accum::Bool {
                    trues: other_trues,
                    falses: other_falses,
                },
            ) => {
                *trues += other_trues;
                *falses += other_falses;
            }
            (
                Accum::SimpleNumber { accum, non_nulls },
                Accum::SimpleNumber {
                    accum: other_accum,
                    non_nulls: other_non_nulls,
                },
            ) => {
                *accum += other_accum;
                *non_nulls += other_non_nulls;
            }
            (
                Accum::Float {
                    accum,
                    pos_infs,
                    neg_infs,
                    nans,
                    non_nulls,
                },
                Accum::Float {
                    accum: other_accum,
                    pos_infs: other_pos_infs,
                    neg_infs: other_neg_infs,
                    nans: other_nans,
                    non_nulls: other_non_nulls,
                },
            ) => {
                *accum = accum.checked_add(*other_accum).unwrap_or_else(|| {
                    warn!("Float accumulator overflow. Incorrect results possible");
                    accum.wrapping_add(*other_accum)
                });
                *pos_infs += other_pos_infs;
                *neg_infs += other_neg_infs;
                *nans += other_nans;
                *non_nulls += other_non_nulls;
            }
            (
                Accum::Numeric {
                    accum,
                    pos_infs,
                    neg_infs,
                    nans,
                    non_nulls,
                },
                Accum::Numeric {
                    accum: other_accum,
                    pos_infs: other_pos_infs,
                    neg_infs: other_neg_infs,
                    nans: other_nans,
                    non_nulls: other_non_nulls,
                },
            ) => {
                let mut cx_agg = numeric::cx_agg();
                cx_agg.add(&mut accum.0, &other_accum.0);
                // `rounded` signals we have exceeded the aggregator's max
                // precision, which means we've lost commutativity and
                // associativity; nothing to be done here, so panic. For more
                // context, see the DEC_Rounded definition at
                // http://speleotrove.com/decimal/dncont.html
                assert!(!cx_agg.status().rounded(), "Accum::Numeric overflow");
                // Reduce to reclaim unused decimal precision. Note that this
                // reduction must happen somewhere to make the following
                // invertible:
                // ```
                // CREATE TABLE a (a numeric);
                // CREATE MATERIALIZED VIEW t as SELECT sum(a) FROM a;
                // INSERT INTO a VALUES ('9e39'), ('9e-39');
                // ```
                // This will now return infinity. However, we can retract the
                // value that blew up its precision:
                // ```
                // INSERT INTO a VALUES ('-9e-39');
                // ```
                // This leaves `t`'s aggregator with a value of 9e39. However,
                // without doing a reduction, `libdecnum` will store the value
                // as 9e39+0e-39, which still exceeds the narrower context's
                // precision. By doing the reduction, we can "reclaim" the 39
                // digits of precision.
                cx_agg.reduce(&mut accum.0);
                *pos_infs += other_pos_infs;
                *neg_infs += other_neg_infs;
                *nans += other_nans;
                *non_nulls += other_non_nulls;
            }
            (l, r) => unreachable!(
                "Accumulator::plus_equals called with non-matching variants: {l:?} vs {r:?}"
            ),
        }
    }
}

impl Multiply<Diff> for Accum {
    type Output = Accum;

    fn multiply(self, factor: &Diff) -> Accum {
        let factor = *factor;
        match self {
            Accum::Bool { trues, falses } => Accum::Bool {
                trues: trues * factor,
                falses: falses * factor,
            },
            Accum::SimpleNumber { accum, non_nulls } => Accum::SimpleNumber {
                accum: accum * i128::from(factor),
                non_nulls: non_nulls * factor,
            },
            Accum::Float {
                accum,
                pos_infs,
                neg_infs,
                nans,
                non_nulls,
            } => Accum::Float {
                accum: accum.checked_mul(i128::from(factor)).unwrap_or_else(|| {
                    warn!("Float accumulator overflow. Incorrect results possible");
                    accum.wrapping_mul(i128::from(factor))
                }),
                pos_infs: pos_infs * factor,
                neg_infs: neg_infs * factor,
                nans: nans * factor,
                non_nulls: non_nulls * factor,
            },
            Accum::Numeric {
                accum,
                pos_infs,
                neg_infs,
                nans,
                non_nulls,
            } => {
                let mut cx = numeric::cx_agg();
                let mut f = NumericAgg::from(factor);
                // Unlike `plus_equals`, not necessary to reduce after this operation because `f` will
                // always be an integer, i.e. we are never increasing the
                // values' scale.
                cx.mul(&mut f, &accum.0);
                // `rounded` signals we have exceeded the aggregator's max
                // precision, which means we've lost commutativity and
                // associativity; nothing to be done here, so panic. For more
                // context, see the DEC_Rounded definition at
                // http://speleotrove.com/decimal/dncont.html
                assert!(!cx.status().rounded(), "Accum::Numeric multiply overflow");
                Accum::Numeric {
                    accum: OrderedDecimal(f),
                    pos_infs: pos_infs * factor,
                    neg_infs: neg_infs * factor,
                    nans: nans * factor,
                    non_nulls: non_nulls * factor,
                }
            }
        }
    }
}

impl Columnation for Accum {
    type InnerRegion = CopyRegion<Self>;
}

/// Monoids for in-place compaction of monotonic streams.
mod monoids {

    // We can improve the performance of some aggregations through the use of algebra.
    // In particular, we can move some of the aggregations in to the `diff` field of
    // updates, by changing `diff` from integers to a different algebraic structure.
    //
    // The one we use is called a "semigroup", and it means that the structure has a
    // symmetric addition operator. The trait we use also allows the semigroup elements
    // to present as "zero", meaning they always act as the identity under +. Here,
    // `Datum::Null` acts as the identity under +, _but_ we don't want to make this
    // known to DD by the `is_zero` method, see comment there. So, from the point of view
    // of DD, this Semigroup should _not_ have a zero.
    //
    // WARNING: `Datum::Null` should continue to act as the identity of our + (even if we
    // add a new enum variant here), because other code (e.g., `HierarchicalOneByOneAggr`)
    // assumes this.

    use differential_dataflow::difference::{Multiply, Semigroup};
    use mz_expr::AggregateFunc;
    use mz_ore::soft_panic_or_log;
    use mz_repr::{Datum, Diff, Row};
    use serde::{Deserialize, Serialize};
    use timely::container::columnation::{Columnation, Region};

    /// A monoid containing a single-datum row.
    #[derive(Ord, PartialOrd, Eq, PartialEq, Debug, Clone, Serialize, Deserialize, Hash)]
    pub enum ReductionMonoid {
        Min(Row),
        Max(Row),
    }

    impl ReductionMonoid {
        pub fn finalize(&self) -> &Row {
            use ReductionMonoid::*;
            match self {
                Min(row) | Max(row) => row,
            }
        }
    }

    impl Multiply<Diff> for ReductionMonoid {
        type Output = Self;

        fn multiply(self, factor: &Diff) -> Self {
            // Multiplication in ReductionMonoid is idempotent, and
            // its users must ascertain its monotonicity beforehand
            // (typically with ensure_monotonic) since it has no zero
            // value for us to use here.
            assert!(factor.is_positive());
            self
        }
    }

    impl Semigroup for ReductionMonoid {
        fn plus_equals(&mut self, rhs: &Self) {
            match (self, rhs) {
                (ReductionMonoid::Min(lhs), ReductionMonoid::Min(rhs)) => {
                    let swap = {
                        let lhs_val = lhs.unpack_first();
                        let rhs_val = rhs.unpack_first();
                        // Datum::Null is the identity, not a small element.
                        match (lhs_val, rhs_val) {
                            (_, Datum::Null) => false,
                            (Datum::Null, _) => true,
                            (lhs, rhs) => rhs < lhs,
                        }
                    };
                    if swap {
                        lhs.clone_from(rhs);
                    }
                }
                (ReductionMonoid::Max(lhs), ReductionMonoid::Max(rhs)) => {
                    let swap = {
                        let lhs_val = lhs.unpack_first();
                        let rhs_val = rhs.unpack_first();
                        // Datum::Null is the identity, not a large element.
                        match (lhs_val, rhs_val) {
                            (_, Datum::Null) => false,
                            (Datum::Null, _) => true,
                            (lhs, rhs) => rhs > lhs,
                        }
                    };
                    if swap {
                        lhs.clone_from(rhs);
                    }
                }
                (lhs, rhs) => {
                    soft_panic_or_log!(
                        "Mismatched monoid variants in reduction! lhs: {lhs:?} rhs: {rhs:?}"
                    );
                }
            }
        }
        fn is_zero(&self) -> bool {
            // It totally looks like we could return true here for `Datum::Null`, but don't do this!
            // DD uses true results of this method to make stuff disappear. This makes sense when
            // diffs mean really just diffs, but for `ReductionMonoid` diffs hold reduction results.
            // We don't want funny stuff, like disappearing, happening to reduction results even
            // when they are null. (This would confuse, e.g., `ReduceCollation` for null inputs.)
            false
        }
    }

    impl Columnation for ReductionMonoid {
        type InnerRegion = ReductionMonoidRegion;
    }

    /// Region for [`ReductionMonoid`]. This region is special in that it stores both enum variants
    /// in the same backing region. Alternatively, it could store it in two regions, but we select
    /// the former for simplicity reasons.
    #[derive(Default)]
    pub struct ReductionMonoidRegion {
        inner: <Row as Columnation>::InnerRegion,
    }

    impl Region for ReductionMonoidRegion {
        type Item = ReductionMonoid;

        unsafe fn copy(&mut self, item: &Self::Item) -> Self::Item {
            use ReductionMonoid::*;
            match item {
                Min(row) => Min(self.inner.copy(row)),
                Max(row) => Max(self.inner.copy(row)),
            }
        }

        fn clear(&mut self) {
            self.inner.clear();
        }

        fn reserve_items<'a, I>(&mut self, items: I)
        where
            Self: 'a,
            I: Iterator<Item = &'a Self::Item> + Clone,
        {
            self.inner
                .reserve_items(items.map(ReductionMonoid::finalize));
        }

        fn reserve_regions<'a, I>(&mut self, regions: I)
        where
            Self: 'a,
            I: Iterator<Item = &'a Self> + Clone,
        {
            self.inner.reserve_regions(regions.map(|r| &r.inner));
        }

        fn heap_size(&self, callback: impl FnMut(usize, usize)) {
            self.inner.heap_size(callback);
        }
    }

    /// Get the correct monoid implementation for a given aggregation function. Note that
    /// all hierarchical aggregation functions need to supply a monoid implementation.
    pub fn get_monoid(row: Row, func: &AggregateFunc) -> Option<ReductionMonoid> {
        match func {
            AggregateFunc::MaxNumeric
            | AggregateFunc::MaxInt16
            | AggregateFunc::MaxInt32
            | AggregateFunc::MaxInt64
            | AggregateFunc::MaxUInt16
            | AggregateFunc::MaxUInt32
            | AggregateFunc::MaxUInt64
            | AggregateFunc::MaxMzTimestamp
            | AggregateFunc::MaxFloat32
            | AggregateFunc::MaxFloat64
            | AggregateFunc::MaxBool
            | AggregateFunc::MaxString
            | AggregateFunc::MaxDate
            | AggregateFunc::MaxTimestamp
            | AggregateFunc::MaxTimestampTz
            | AggregateFunc::MaxInterval => Some(ReductionMonoid::Max(row)),
            AggregateFunc::MinNumeric
            | AggregateFunc::MinInt16
            | AggregateFunc::MinInt32
            | AggregateFunc::MinInt64
            | AggregateFunc::MinUInt16
            | AggregateFunc::MinUInt32
            | AggregateFunc::MinUInt64
            | AggregateFunc::MinMzTimestamp
            | AggregateFunc::MinFloat32
            | AggregateFunc::MinFloat64
            | AggregateFunc::MinBool
            | AggregateFunc::MinString
            | AggregateFunc::MinDate
            | AggregateFunc::MinTimestamp
            | AggregateFunc::MinTimestampTz
            | AggregateFunc::MinInterval => Some(ReductionMonoid::Min(row)),
            AggregateFunc::SumInt16
            | AggregateFunc::SumInt32
            | AggregateFunc::SumInt64
            | AggregateFunc::SumUInt16
            | AggregateFunc::SumUInt32
            | AggregateFunc::SumUInt64
            | AggregateFunc::SumFloat32
            | AggregateFunc::SumFloat64
            | AggregateFunc::SumNumeric
            | AggregateFunc::Count
            | AggregateFunc::Any
            | AggregateFunc::All
            | AggregateFunc::Dummy
            | AggregateFunc::JsonbAgg { .. }
            | AggregateFunc::JsonbObjectAgg { .. }
            | AggregateFunc::ArrayConcat { .. }
            | AggregateFunc::ListConcat { .. }
            | AggregateFunc::StringAgg { .. }
            | AggregateFunc::RowNumber { .. }
            | AggregateFunc::Rank { .. }
            | AggregateFunc::DenseRank { .. }
            | AggregateFunc::LagLead { .. }
            | AggregateFunc::FirstValue { .. }
            | AggregateFunc::LastValue { .. }
            | AggregateFunc::WindowAggregate { .. } => None,
        }
    }
}

mod window_agg_helpers {
    use crate::render::reduce::*;

    /// TODO: It would be better for performance to do the branching that is in the methods of this
    /// enum at the place where we are calling `eval_fast_window_agg`. Then we wouldn't need an enum
    /// here, and would parameterize `eval_fast_window_agg` with one of the implementations
    /// directly.
    pub enum OneByOneAggrImpls {
        Accumulable(AccumulableOneByOneAggr),
        Hierarchical(HierarchicalOneByOneAggr),
        Basic(mz_expr::NaiveOneByOneAggr),
    }

    impl mz_expr::OneByOneAggr for OneByOneAggrImpls {
        fn new(agg: &AggregateFunc, reverse: bool) -> Self {
            match reduction_type(agg) {
                ReductionType::Basic => {
                    OneByOneAggrImpls::Basic(mz_expr::NaiveOneByOneAggr::new(agg, reverse))
                }
                ReductionType::Accumulable => {
                    OneByOneAggrImpls::Accumulable(AccumulableOneByOneAggr::new(agg))
                }
                ReductionType::Hierarchical => {
                    OneByOneAggrImpls::Hierarchical(HierarchicalOneByOneAggr::new(agg))
                }
            }
        }

        fn give(&mut self, d: &Datum) {
            match self {
                OneByOneAggrImpls::Basic(i) => i.give(d),
                OneByOneAggrImpls::Accumulable(i) => i.give(d),
                OneByOneAggrImpls::Hierarchical(i) => i.give(d),
            }
        }

        fn get_current_aggregate<'a>(&self, temp_storage: &'a RowArena) -> Datum<'a> {
            // Note that the `reverse` parameter is currently forwarded only for Basic aggregations.
            match self {
                OneByOneAggrImpls::Basic(i) => i.get_current_aggregate(temp_storage),
                OneByOneAggrImpls::Accumulable(i) => i.get_current_aggregate(temp_storage),
                OneByOneAggrImpls::Hierarchical(i) => i.get_current_aggregate(temp_storage),
            }
        }
    }

    pub struct AccumulableOneByOneAggr {
        aggr_func: AggregateFunc,
        accum: Accum,
        total: Diff,
    }

    impl AccumulableOneByOneAggr {
        fn new(aggr_func: &AggregateFunc) -> Self {
            AccumulableOneByOneAggr {
                aggr_func: aggr_func.clone(),
                accum: accumulable_zero(aggr_func),
                total: 0,
            }
        }

        fn give(&mut self, d: &Datum) {
            self.accum
                .plus_equals(&datum_to_accumulator(&self.aggr_func, d.clone()));
            self.total += 1;
        }

        fn get_current_aggregate<'a>(&self, temp_storage: &'a RowArena) -> Datum<'a> {
            temp_storage.make_datum(|packer| {
                packer.push(finalize_accum(&self.aggr_func, &self.accum, self.total));
            })
        }
    }

    pub struct HierarchicalOneByOneAggr {
        aggr_func: AggregateFunc,
        // Warning: We are assuming that `Datum::Null` acts as the identity for `ReductionMonoid`'s
        // `plus_equals`. (But _not_ relying here on `ReductionMonoid::is_zero`.)
        monoid: ReductionMonoid,
    }

    impl HierarchicalOneByOneAggr {
        fn new(aggr_func: &AggregateFunc) -> Self {
            let mut row_buf = Row::default();
            row_buf.packer().push(Datum::Null);
            HierarchicalOneByOneAggr {
                aggr_func: aggr_func.clone(),
                monoid: get_monoid(row_buf, aggr_func)
                    .expect("aggr_func should be a hierarchical aggregation function"),
            }
        }

        fn give(&mut self, d: &Datum) {
            let mut row_buf = Row::default();
            row_buf.packer().push(d);
            let m = get_monoid(row_buf, &self.aggr_func)
                .expect("aggr_func should be a hierarchical aggregation function");
            self.monoid.plus_equals(&m);
        }

        fn get_current_aggregate<'a>(&self, temp_storage: &'a RowArena) -> Datum<'a> {
            temp_storage.make_datum(|packer| packer.extend(self.monoid.finalize().iter()))
        }
    }
}

mod functions;
mod generic;
mod group_by;
mod hconcat;
mod hstack;
mod joins;
mod projection;
#[cfg(feature = "semi_anti_join")]
mod semi_anti_join;

use polars_core::datatypes::PlHashSet;
use polars_core::prelude::*;
use polars_io::RowIndex;
use polars_utils::idx_vec::UnitVec;
use recursive::recursive;
#[cfg(feature = "semi_anti_join")]
use semi_anti_join::process_semi_anti_join;

use crate::prelude::optimizer::projection_pushdown::generic::process_generic;
use crate::prelude::optimizer::projection_pushdown::group_by::process_group_by;
use crate::prelude::optimizer::projection_pushdown::hconcat::process_hconcat;
use crate::prelude::optimizer::projection_pushdown::hstack::process_hstack;
use crate::prelude::optimizer::projection_pushdown::joins::process_join;
use crate::prelude::optimizer::projection_pushdown::projection::process_projection;
use crate::prelude::*;
use crate::utils::aexpr_to_leaf_names;

#[derive(Default, Copy, Clone)]
struct ProjectionCopyState {
    projections_seen: usize,
    is_count_star: bool,
}

#[derive(Clone, Default)]
struct ProjectionContext {
    acc_projections: Vec<ColumnNode>,
    projected_names: PlHashSet<PlSmallStr>,
    inner: ProjectionCopyState,
}

impl ProjectionContext {
    fn new(
        acc_projections: Vec<ColumnNode>,
        projected_names: PlHashSet<PlSmallStr>,
        inner: ProjectionCopyState,
    ) -> Self {
        Self {
            acc_projections,
            projected_names,
            inner,
        }
    }

    /// If this is `true`, other nodes should add the columns
    /// they need to the push down state
    fn has_pushed_down(&self) -> bool {
        // count star also acts like a pushdown as we will select a single column at the source
        // when there were no other projections.
        !self.acc_projections.is_empty() || self.inner.is_count_star
    }

    fn process_count_star_at_scan(&mut self, schema: &Schema, expr_arena: &mut Arena<AExpr>) {
        if self.acc_projections.is_empty() {
            let (name, _dt) = match schema.len() {
                0 => return,
                1 => schema.get_at_index(0).unwrap(),
                _ => {
                    // skip first as that can be the row index.
                    // We look for a relative cheap type, such as a numeric or bool
                    schema
                        .iter()
                        .skip(1)
                        .find(|(_name, dt)| {
                            let phys = dt;
                            phys.is_null()
                                || phys.is_primitive_numeric()
                                || phys.is_bool()
                                || phys.is_temporal()
                        })
                        .unwrap_or_else(|| schema.get_at_index(schema.len() - 1).unwrap())
                },
            };

            let node = expr_arena.add(AExpr::Column(name.clone()));
            self.acc_projections.push(ColumnNode(node));
            self.projected_names.insert(name.clone());
        }
    }
}

/// utility function to get names of the columns needed in projection at scan level
fn get_scan_columns(
    acc_projections: &[ColumnNode],
    expr_arena: &Arena<AExpr>,
    row_index: Option<&RowIndex>,
    file_path_col: Option<&str>,
    // When set, the column order will match the order from the provided schema
    normalize_order_schema: Option<&Schema>,
) -> Option<Arc<[PlSmallStr]>> {
    if acc_projections.is_empty() {
        return None;
    }

    let mut column_names = acc_projections
        .iter()
        .filter_map(|node| {
            let name = column_node_to_name(*node, expr_arena);

            if let Some(ri) = row_index {
                if ri.name == name {
                    return None;
                }
            }

            if let Some(file_path_col) = file_path_col {
                if file_path_col == name.as_str() {
                    return None;
                }
            }

            Some(name.clone())
        })
        .collect::<Vec<_>>();

    if let Some(schema) = normalize_order_schema {
        column_names.sort_unstable_by_key(|name| schema.try_get_full(name).unwrap().0);
    }

    Some(column_names.into_iter().collect::<Arc<[_]>>())
}

/// split in a projection vec that can be pushed down and a projection vec that should be used
/// in this node
///
/// # Returns
/// accumulated_projections, local_projections, accumulated_names
///
/// - `expands_schema`. An unnest adds more columns to a schema, so we cannot use fast path
fn split_acc_projections(
    acc_projections: Vec<ColumnNode>,
    down_schema: &Schema,
    expr_arena: &Arena<AExpr>,
    expands_schema: bool,
) -> (Vec<ColumnNode>, Vec<ColumnNode>, PlHashSet<PlSmallStr>) {
    // If node above has as many columns as the projection there is nothing to pushdown.
    if !expands_schema && down_schema.len() == acc_projections.len() {
        let local_projections = acc_projections;
        (vec![], local_projections, PlHashSet::new())
    } else {
        let (acc_projections, local_projections): (Vec<_>, Vec<_>) = acc_projections
            .into_iter()
            .partition(|expr| check_input_column_node(*expr, down_schema, expr_arena));
        let mut names = PlHashSet::default();
        for proj in &acc_projections {
            let name = column_node_to_name(*proj, expr_arena).clone();
            names.insert(name);
        }
        (acc_projections, local_projections, names)
    }
}

/// utility function such that we can recurse all binary expressions in the expression tree
fn add_expr_to_accumulated(
    expr: Node,
    acc_projections: &mut Vec<ColumnNode>,
    projected_names: &mut PlHashSet<PlSmallStr>,
    expr_arena: &Arena<AExpr>,
) {
    for root_node in aexpr_to_column_nodes_iter(expr, expr_arena) {
        let name = column_node_to_name(root_node, expr_arena).clone();
        if projected_names.insert(name) {
            acc_projections.push(root_node)
        }
    }
}

fn add_str_to_accumulated(
    name: PlSmallStr,
    ctx: &mut ProjectionContext,
    expr_arena: &mut Arena<AExpr>,
) {
    // if not pushed down: all columns are already projected.
    if ctx.has_pushed_down() && !ctx.projected_names.contains(&name) {
        let node = expr_arena.add(AExpr::Column(name));
        add_expr_to_accumulated(
            node,
            &mut ctx.acc_projections,
            &mut ctx.projected_names,
            expr_arena,
        );
    }
}

fn update_scan_schema(
    acc_projections: &[ColumnNode],
    expr_arena: &Arena<AExpr>,
    schema: &Schema,
    sort_projections: bool,
) -> PolarsResult<Schema> {
    let mut new_schema = Schema::with_capacity(acc_projections.len());
    let mut new_cols = Vec::with_capacity(acc_projections.len());
    for node in acc_projections.iter() {
        let name = column_node_to_name(*node, expr_arena);
        let item = schema.try_get_full(name)?;
        new_cols.push(item);
    }
    // make sure that the projections are sorted by the schema.
    if sort_projections {
        new_cols.sort_unstable_by_key(|item| item.0);
    }
    for item in new_cols {
        new_schema.with_column(item.1.clone(), item.2.clone());
    }
    Ok(new_schema)
}

pub struct ProjectionPushDown {
    pub is_count_star: bool,
}

impl ProjectionPushDown {
    pub(super) fn new() -> Self {
        Self {
            is_count_star: false,
        }
    }

    /// Projection will be done at this node, but we continue optimization
    fn no_pushdown_restart_opt(
        &mut self,
        lp: IR,
        ctx: ProjectionContext,
        lp_arena: &mut Arena<IR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<IR> {
        let inputs = lp.get_inputs();

        let new_inputs = inputs
            .into_iter()
            .map(|node| {
                let alp = lp_arena.take(node);
                let ctx = ProjectionContext::new(Default::default(), Default::default(), ctx.inner);
                let alp = self.push_down(alp, ctx, lp_arena, expr_arena)?;
                lp_arena.replace(node, alp);
                Ok(node)
            })
            .collect::<PolarsResult<UnitVec<_>>>()?;
        let lp = lp.with_inputs(new_inputs);

        let builder = IRBuilder::from_lp(lp, expr_arena, lp_arena);
        Ok(self.finish_node_simple_projection(&ctx.acc_projections, builder))
    }

    fn finish_node_simple_projection(
        &mut self,
        local_projections: &[ColumnNode],
        builder: IRBuilder,
    ) -> IR {
        if !local_projections.is_empty() {
            builder
                .project_simple_nodes(local_projections.iter().map(|node| node.0))
                .unwrap()
                .build()
        } else {
            builder.build()
        }
    }

    fn finish_node(&mut self, local_projections: Vec<ExprIR>, builder: IRBuilder) -> IR {
        if !local_projections.is_empty() {
            builder
                .project(local_projections, Default::default())
                .build()
        } else {
            builder.build()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn join_push_down(
        &mut self,
        schema_left: &Schema,
        schema_right: &Schema,
        proj: ColumnNode,
        pushdown_left: &mut Vec<ColumnNode>,
        pushdown_right: &mut Vec<ColumnNode>,
        names_left: &mut PlHashSet<PlSmallStr>,
        names_right: &mut PlHashSet<PlSmallStr>,
        expr_arena: &Arena<AExpr>,
    ) -> (bool, bool) {
        let mut pushed_at_least_one = false;
        let mut already_projected = false;

        let name = column_node_to_name(proj, expr_arena);
        let is_in_left = names_left.contains(name);
        let is_in_right = names_right.contains(name);
        already_projected |= is_in_left;
        already_projected |= is_in_right;

        if check_input_column_node(proj, schema_left, expr_arena) && !is_in_left {
            names_left.insert(name.clone());
            pushdown_left.push(proj);
            pushed_at_least_one = true;
        }
        if check_input_column_node(proj, schema_right, expr_arena) && !is_in_right {
            names_right.insert(name.clone());
            pushdown_right.push(proj);
            pushed_at_least_one = true;
        }

        (pushed_at_least_one, already_projected)
    }

    /// This pushes down current node and assigns the result to this node.
    fn pushdown_and_assign(
        &mut self,
        input: Node,
        ctx: ProjectionContext,
        lp_arena: &mut Arena<IR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<()> {
        let alp = lp_arena.take(input);
        let lp = self.push_down(alp, ctx, lp_arena, expr_arena)?;
        lp_arena.replace(input, lp);
        Ok(())
    }

    /// This pushes down the projection that are validated
    /// that they can be done successful at the schema above
    /// The result is assigned to this node.
    ///
    /// The local projections are return and still have to be applied
    fn pushdown_and_assign_check_schema(
        &mut self,
        input: Node,
        mut ctx: ProjectionContext,
        lp_arena: &mut Arena<IR>,
        expr_arena: &mut Arena<AExpr>,
        // an unnest changes/expands the schema
        expands_schema: bool,
    ) -> PolarsResult<Vec<ColumnNode>> {
        let alp = lp_arena.take(input);
        let down_schema = alp.schema(lp_arena);

        let (acc_projections, local_projections, names) = split_acc_projections(
            ctx.acc_projections,
            &down_schema,
            expr_arena,
            expands_schema,
        );

        ctx.acc_projections = acc_projections;
        ctx.projected_names = names;

        let lp = self.push_down(alp, ctx, lp_arena, expr_arena)?;
        lp_arena.replace(input, lp);
        Ok(local_projections)
    }

    /// Projection pushdown optimizer
    ///
    /// # Arguments
    ///
    /// * `IR` - Arena based logical plan tree representing the query.
    /// * `acc_projections` - The projections we accumulate during tree traversal.
    /// * `names` - We keep track of the names to ensure we don't do duplicate projections.
    /// * `projections_seen` - Count the number of projection operations during tree traversal.
    /// * `lp_arena` - The local memory arena for the logical plan.
    /// * `expr_arena` - The local memory arena for the expressions.
    #[recursive]
    fn push_down(
        &mut self,
        logical_plan: IR,
        mut ctx: ProjectionContext,
        lp_arena: &mut Arena<IR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<IR> {
        use IR::*;

        match logical_plan {
            Select { expr, input, .. } => {
                process_projection(self, input, expr, ctx, lp_arena, expr_arena, false)
            },
            SimpleProjection { columns, input, .. } => {
                let exprs = names_to_expr_irs(columns.iter_names_cloned(), expr_arena);
                process_projection(self, input, exprs, ctx, lp_arena, expr_arena, true)
            },
            DataFrameScan {
                df,
                schema,
                mut output_schema,
                ..
            } => {
                // TODO: Just project 0-width morsels.
                if self.is_count_star {
                    ctx.process_count_star_at_scan(&schema, expr_arena);
                }
                if ctx.has_pushed_down() {
                    output_schema = Some(Arc::new(update_scan_schema(
                        &ctx.acc_projections,
                        expr_arena,
                        &schema,
                        false,
                    )?));
                }
                let lp = DataFrameScan {
                    df,
                    schema,
                    output_schema,
                };
                Ok(lp)
            },
            #[cfg(feature = "python")]
            PythonScan { mut options } => {
                if self.is_count_star {
                    ctx.process_count_star_at_scan(&options.schema, expr_arena);
                }

                let normalize_order_schema = Some(&*options.schema);

                options.with_columns = get_scan_columns(
                    &ctx.acc_projections,
                    expr_arena,
                    None,
                    None,
                    normalize_order_schema,
                );

                options.output_schema = if options.with_columns.is_none() {
                    None
                } else {
                    Some(Arc::new(update_scan_schema(
                        &ctx.acc_projections,
                        expr_arena,
                        &options.schema,
                        true,
                    )?))
                };
                Ok(PythonScan { options })
            },
            Scan {
                sources,
                mut file_info,
                hive_parts,
                scan_type,
                predicate,
                mut unified_scan_args,
                mut output_schema,
            } => {
                let do_optimization = match &*scan_type {
                    FileScanIR::Anonymous { function, .. } => function.allows_projection_pushdown(),
                    #[cfg(feature = "json")]
                    FileScanIR::NDJson { .. } => true,
                    #[cfg(feature = "ipc")]
                    FileScanIR::Ipc { .. } => true,
                    #[cfg(feature = "csv")]
                    FileScanIR::Csv { .. } => true,
                    #[cfg(feature = "parquet")]
                    FileScanIR::Parquet { .. } => true,
                    // MultiScan will handle it if the PythonDataset cannot do projections.
                    #[cfg(feature = "python")]
                    FileScanIR::PythonDataset { .. } => true,
                };

                #[expect(clippy::never_loop)]
                loop {
                    if !do_optimization {
                        break;
                    }

                    if self.is_count_star {
                        if let FileScanIR::Anonymous { .. } = &*scan_type {
                            // Anonymous scan is not controlled by us, we don't know if it can support
                            // 0-column projections, so we always project one.
                            use either::Either;

                            let projection: Arc<[PlSmallStr]> = match &file_info.reader_schema {
                                Some(Either::Left(s)) => s.iter_names().next(),
                                Some(Either::Right(s)) => s.iter_names().next(),
                                None => None,
                            }
                            .into_iter()
                            .cloned()
                            .collect();

                            unified_scan_args.projection = Some(projection.clone());

                            if projection.is_empty() {
                                output_schema = Some(Default::default());
                                break;
                            }

                            ctx.acc_projections.push(ColumnNode(
                                expr_arena.add(AExpr::Column(projection[0].clone())),
                            ));

                            unified_scan_args.projection = Some(projection)
                        } else {
                            // All nodes in new-streaming support projecting empty morsels with the correct height
                            // from the file.
                            unified_scan_args.projection = Some(Arc::from([]));
                            output_schema = Some(Default::default());
                            break;
                        };
                    }

                    unified_scan_args.projection = get_scan_columns(
                        &ctx.acc_projections,
                        expr_arena,
                        unified_scan_args.row_index.as_ref(),
                        unified_scan_args.include_file_paths.as_deref(),
                        None,
                    );

                    output_schema = if unified_scan_args.projection.is_some() {
                        let mut schema = update_scan_schema(
                            &ctx.acc_projections,
                            expr_arena,
                            &file_info.schema,
                            scan_type.sort_projection(unified_scan_args.row_index.is_some()),
                        )?;

                        if let Some(ref file_path_col) = unified_scan_args.include_file_paths {
                            if let Some(i) = schema.index_of(file_path_col) {
                                let (name, dtype) = schema.shift_remove_index(i).unwrap();
                                schema.insert_at_index(schema.len(), name, dtype)?;
                            }
                        }

                        Some(Arc::new(schema))
                    } else {
                        None
                    };

                    break;
                }

                // File builder has a row index, but projected columns
                // do not include it, so cull.
                if let Some(RowIndex { ref name, .. }) = unified_scan_args.row_index {
                    if output_schema
                        .as_ref()
                        .is_some_and(|schema| !schema.contains(name))
                    {
                        // Need to remove it from the input schema so
                        // that projection indices are correct.
                        let mut file_schema = Arc::unwrap_or_clone(file_info.schema);
                        file_schema.shift_remove(name);
                        file_info.schema = Arc::new(file_schema);
                        unified_scan_args.row_index = None;
                    }
                };

                if let Some(col_name) = &unified_scan_args.include_file_paths {
                    if output_schema
                        .as_ref()
                        .is_some_and(|schema| !schema.contains(col_name))
                    {
                        // Need to remove it from the input schema so
                        // that projection indices are correct.
                        let mut file_schema = Arc::unwrap_or_clone(file_info.schema);
                        file_schema.shift_remove(col_name);
                        file_info.schema = Arc::new(file_schema);
                        unified_scan_args.include_file_paths = None;
                    }
                };

                let lp = Scan {
                    sources,
                    file_info,
                    hive_parts,
                    output_schema,
                    scan_type,
                    predicate,
                    unified_scan_args,
                };

                Ok(lp)
            },
            Sort {
                input,
                by_column,
                slice,
                sort_options,
            } => {
                if ctx.has_pushed_down() {
                    // Make sure that the column(s) used for the sort is projected
                    by_column.iter().for_each(|node| {
                        add_expr_to_accumulated(
                            node.node(),
                            &mut ctx.acc_projections,
                            &mut ctx.projected_names,
                            expr_arena,
                        );
                    });
                }

                self.pushdown_and_assign(input, ctx, lp_arena, expr_arena)?;
                Ok(Sort {
                    input,
                    by_column,
                    slice,
                    sort_options,
                })
            },
            Distinct { input, options } => {
                // make sure that the set of unique columns is projected
                if ctx.has_pushed_down() {
                    if let Some(subset) = options.subset.as_ref() {
                        subset.iter().for_each(|name| {
                            add_str_to_accumulated(name.clone(), &mut ctx, expr_arena)
                        })
                    } else {
                        // distinct needs all columns
                        let input_schema = lp_arena.get(input).schema(lp_arena);
                        for name in input_schema.iter_names() {
                            add_str_to_accumulated(name.clone(), &mut ctx, expr_arena)
                        }
                    }
                }

                self.pushdown_and_assign(input, ctx, lp_arena, expr_arena)?;
                Ok(Distinct { input, options })
            },
            Filter { predicate, input } => {
                if ctx.has_pushed_down() {
                    // make sure that the filter column is projected
                    add_expr_to_accumulated(
                        predicate.node(),
                        &mut ctx.acc_projections,
                        &mut ctx.projected_names,
                        expr_arena,
                    );
                };
                self.pushdown_and_assign(input, ctx, lp_arena, expr_arena)?;
                Ok(Filter { predicate, input })
            },
            GroupBy {
                input,
                keys,
                aggs,
                apply,
                schema,
                maintain_order,
                options,
            } => process_group_by(
                self,
                input,
                keys,
                aggs,
                apply,
                schema,
                maintain_order,
                options,
                ctx,
                lp_arena,
                expr_arena,
            ),
            Join {
                input_left,
                input_right,
                left_on,
                right_on,
                options,
                schema,
            } => match options.args.how {
                #[cfg(feature = "semi_anti_join")]
                JoinType::Semi | JoinType::Anti => process_semi_anti_join(
                    self,
                    input_left,
                    input_right,
                    left_on,
                    right_on,
                    options,
                    ctx,
                    lp_arena,
                    expr_arena,
                ),
                _ => process_join(
                    self,
                    input_left,
                    input_right,
                    left_on,
                    right_on,
                    options,
                    ctx,
                    lp_arena,
                    expr_arena,
                    &schema,
                ),
            },
            HStack {
                input,
                exprs,
                options,
                ..
            } => process_hstack(self, input, exprs, options, ctx, lp_arena, expr_arena),
            ExtContext {
                input, contexts, ..
            } => {
                // local projections are ignored. These are just root nodes
                // complex expression will still be done later
                let _local_projections =
                    self.pushdown_and_assign_check_schema(input, ctx, lp_arena, expr_arena, false)?;

                let mut new_schema = lp_arena
                    .get(input)
                    .schema(lp_arena)
                    .as_ref()
                    .as_ref()
                    .clone();

                for node in &contexts {
                    let other_schema = lp_arena.get(*node).schema(lp_arena);
                    for fld in other_schema.iter_fields() {
                        if new_schema.get(fld.name()).is_none() {
                            new_schema.with_column(fld.name, fld.dtype);
                        }
                    }
                }

                Ok(ExtContext {
                    input,
                    contexts,
                    schema: Arc::new(new_schema),
                })
            },
            MapFunction { input, function } => {
                functions::process_functions(self, input, function, ctx, lp_arena, expr_arena)
            },
            HConcat {
                inputs,
                schema,
                options,
            } => process_hconcat(self, inputs, schema, options, ctx, lp_arena, expr_arena),
            lp @ Union { .. } => process_generic(self, lp, ctx, lp_arena, expr_arena),
            // These nodes only have inputs and exprs, so we can use same logic.
            lp @ Slice { .. } | lp @ Sink { .. } | lp @ SinkMultiple { .. } => {
                process_generic(self, lp, ctx, lp_arena, expr_arena)
            },
            Cache { .. } => {
                // projections above this cache will be accumulated and pushed down
                // later
                // the redundant projection will be cleaned in the fast projection optimization
                // phase.
                if ctx.acc_projections.is_empty() {
                    Ok(logical_plan)
                } else {
                    Ok(IRBuilder::from_lp(logical_plan, expr_arena, lp_arena)
                        .project_simple_nodes(ctx.acc_projections)
                        .unwrap()
                        .build())
                }
            },
            #[cfg(feature = "merge_sorted")]
            MergeSorted {
                input_left,
                input_right,
                key,
            } => {
                if ctx.has_pushed_down() {
                    // make sure that the filter column is projected
                    add_str_to_accumulated(key.clone(), &mut ctx, expr_arena);
                };

                self.pushdown_and_assign(input_left, ctx.clone(), lp_arena, expr_arena)?;
                self.pushdown_and_assign(input_right, ctx, lp_arena, expr_arena)?;

                Ok(MergeSorted {
                    input_left,
                    input_right,
                    key,
                })
            },
            Invalid => unreachable!(),
        }
    }

    pub fn optimize(
        &mut self,
        logical_plan: IR,
        lp_arena: &mut Arena<IR>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<IR> {
        let ctx = ProjectionContext::default();
        self.push_down(logical_plan, ctx, lp_arena, expr_arena)
    }
}

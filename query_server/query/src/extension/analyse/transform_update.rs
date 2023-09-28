use std::sync::Arc;

use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::common::Result as DFResult;
use datafusion::config::ConfigOptions;
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{Extension, LogicalPlan, LogicalPlanBuilder};
use datafusion::optimizer::analyzer::AnalyzerRule;
use datafusion::prelude::{col, Expr};
use models::schema::TskvTableSchema;
use spi::query::logical_planner::{affected_row_expr, merge_affected_row_expr};
use spi::QueryError;

use crate::data_source::table_source::{TableHandle, TableSourceAdapter};
use crate::extension::logical::logical_plan_builder::LogicalPlanBuilderExt;
use crate::extension::logical::plan_node::table_writer_merge::TableWriterMergePlanNode;
use crate::extension::logical::plan_node::update::UpdateNode;
use crate::extension::logical::plan_node::update_tag::UpdateTagPlanNode;
use crate::extension::utils::{downcast_plan_node, downcast_table_source};

#[derive(Default)]
#[non_exhaustive]
pub struct TransformUpdateRule {}

impl TransformUpdateRule {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AnalyzerRule for TransformUpdateRule {
    fn analyze(&self, plan: LogicalPlan, _config: &ConfigOptions) -> DFResult<LogicalPlan> {
        plan.transform_up(&analyze_internal)
    }

    fn name(&self) -> &str {
        "transform_update"
    }
}

fn analyze_internal(plan: LogicalPlan) -> DFResult<Transformed<LogicalPlan>> {
    if let LogicalPlan::Extension(Extension { node }) = &plan {
        if let Some(
            update_node @ UpdateNode {
                table_source,
                assigns,
                ..
            },
        ) = downcast_plan_node::<UpdateNode>(node.as_ref())
        {
            let source_adapter = downcast_table_source::<TableSourceAdapter>(table_source.as_ref())
                .ok_or_else(|| {
                    DataFusionError::External(Box::new(QueryError::Internal {
                        reason:
                            "The implementation of TableSource in cnosdb must be TableSourceAdapter"
                                .to_string(),
                    }))
                })?;

            match source_adapter.table_handle() {
                TableHandle::Tskv(table) => {
                    let schema = table.table_schema();
                    let columns = assigns
                        .iter()
                        .map(|(col, _)| {
                            schema.column(&col.name).ok_or_else(|| {
                                DataFusionError::External(Box::new(QueryError::Internal {
                                    reason: format!("Column {} not found in table", col.name),
                                }))
                            })
                        })
                        .collect::<DFResult<Vec<_>>>()?;

                    let is_update_time = columns.iter().all(|c| c.column_type.is_time());
                    if is_update_time {
                        return update_time().map(Transformed::Yes);
                    }

                    let is_update_tag = columns.iter().all(|c| c.column_type.is_tag());
                    if is_update_tag {
                        return update_tag(update_node, schema).map(Transformed::Yes);
                    }

                    let is_update_field = columns.iter().all(|c| c.column_type.is_field());
                    if is_update_field {
                        return update_field(update_node).map(Transformed::Yes);
                    }

                    return Err(DataFusionError::External(Box::new(QueryError::Analyzer {
                        err: "Update the time/tag/field columns at the same statement is not supported".to_string(),
                    })));
                }
                _ => {
                    return Err(DataFusionError::External(Box::new(QueryError::Analyzer {
                        err: "Only support update tskv table".to_string(),
                    })))
                }
            }
        }
    }

    Ok(Transformed::No(plan))
}

/// 不支持update time
fn update_time() -> DFResult<LogicalPlan> {
    Err(DataFusionError::External(Box::new(
        QueryError::NotImplemented {
            err: "update_time".to_string(),
        },
    )))
}

/// 生成update tag的逻辑计划
fn update_tag(update_node: &UpdateNode, schema: Arc<TskvTableSchema>) -> DFResult<LogicalPlan> {
    let UpdateNode {
        table_name,
        table_source,
        assigns,
        filter,
        ..
    } = update_node;

    let mut projection = vec![];
    for field in schema.columns() {
        if field.column_type.is_tag() || field.column_type.is_time() {
            projection.push(col(field.name.clone()));
        }
    }

    let scan = LogicalPlanBuilder::scan(table_name.clone(), table_source.clone(), None)?
        .filter(filter.clone())?
        .project(projection)?
        .build()?;

    let input_exprs = scan
        .schema()
        .fields()
        .iter()
        .map(|f| Expr::Column(f.qualified_column()))
        .collect::<Vec<Expr>>();
    let affected_row_expr = affected_row_expr(input_exprs);

    let plan = UpdateTagPlanNode::try_new(
        table_name.to_string(),
        table_source.clone(),
        Arc::new(scan),
        assigns.clone(),
        vec![affected_row_expr],
    )?;

    let plan =
        TableWriterMergePlanNode::try_new(Arc::new(plan.into()), vec![merge_affected_row_expr()])?;

    Ok(plan.into())
}

/// 生成update field的逻辑计划
fn update_field(update_node: &UpdateNode) -> DFResult<LogicalPlan> {
    let UpdateNode {
        table_name,
        table_source,
        assigns,
        filter,
        ..
    } = update_node;

    // All columns need to be included (old and updated column values)
    let (insert_columns, project_exprs): (Vec<_>, Vec<_>) = table_source
        .schema()
        .fields()
        .iter()
        .map(|field| {
            let name = field.name();
            let set_value = assigns.iter().find(|(col, _)| &col.name == field.name());
            let expr = match set_value {
                Some((_, expr)) => expr.clone().alias(name),
                None => col(name),
            };
            (name.clone(), expr)
        })
        .unzip();

    let plan = LogicalPlanBuilder::scan(table_name.clone(), table_source.clone(), None)?
        .filter(filter.clone())?
        .project(project_exprs)?
        .write(table_source.clone(), table_name.table(), &insert_columns)?
        .build()?;

    Ok(plan)
}

use std::sync::Arc;

use parking_lot::Mutex;
use polars_core::frame::DataFrame;
use polars_core::schema::Schema;
use polars_error::PolarsResult;
use polars_plan::dsl::{FileScanIR, ScanSources};
use polars_plan::plans::{AnonymousScan, AnonymousScanArgs, FileInfo, IR};
use polars_plan::prelude::{AnonymousScanOptions, UnifiedScanArgs};

/// Used to insert a dataframe into in-memory-engine query plan after the query
/// plan has been made.
#[derive(Default)]
pub struct LateMaterializedDataFrame {
    df: Mutex<Option<DataFrame>>,
}

impl LateMaterializedDataFrame {
    pub fn set_materialized_dataframe(&self, df: DataFrame) {
        *self.df.lock() = Some(df);
    }

    pub fn as_ir_node(self: Arc<Self>, schema: Arc<Schema>) -> IR {
        let options = Arc::new(AnonymousScanOptions {
            skip_rows: None,
            fmt_str: "LateMaterializedDataFrame",
        });
        IR::Scan {
            sources: ScanSources::Paths(Arc::default()),
            file_info: FileInfo::new(schema, None, (None, usize::MAX)),
            hive_parts: None,
            predicate: None,
            output_schema: None,
            scan_type: Box::new(FileScanIR::Anonymous {
                options,
                function: self,
            }),
            unified_scan_args: Box::new(UnifiedScanArgs::default()),
        }
    }
}

impl AnonymousScan for LateMaterializedDataFrame {
    fn as_any(&self) -> &dyn std::any::Any {
        unimplemented!()
    }

    fn scan(&self, _scan_opts: AnonymousScanArgs) -> PolarsResult<DataFrame> {
        Ok(self.df.lock().take().unwrap())
    }
}

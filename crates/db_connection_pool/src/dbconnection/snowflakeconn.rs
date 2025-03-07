/*
Copyright 2024 The Spice.ai OSS Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::any::Any;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, Int32Array, Int64Array, RecordBatch, StructArray, TimestampMillisecondBuilder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use async_trait::async_trait;
use datafusion::error::DataFusionError;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::sql::TableReference;
use futures::stream;
use futures::StreamExt;
use futures::TryStreamExt;
use snafu::prelude::*;
use snowflake_api::SnowflakeApi;

use super::AsyncDbConnection;
use super::DbConnection;
use super::Result;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Not implemented"))]
    NotImplemented,

    #[snafu(display("Unable to retrieve schema: {reason}"))]
    UnableToRetrieveSchema { reason: String },

    #[snafu(display("Unexpected query response, expected Arrow, got JSON: {json}"))]
    UnexpectedResponse { json: String },

    #[snafu(display("Error executing query: {source}"))]
    SnowflakeQueryError {
        source: snowflake_api::SnowflakeApiError,
    },

    #[snafu(display("Error executing query: {source}"))]
    SnowflakeArrowError { source: arrow::error::ArrowError },

    #[snafu(display("Failed to cast snowflake timestamp to arrow timestamp: {reason}"))]
    UnableToCastSnowflakeTimestamp { reason: String },

    #[snafu(display("Failed to create record batch: {source}"))]
    FailedToCreateRecordBatch { source: arrow::error::ArrowError },
}

pub struct SnowflakeConnection {
    pub api: Arc<SnowflakeApi>,
}

impl<'a> DbConnection<Arc<SnowflakeApi>, &'a (dyn Sync)> for SnowflakeConnection {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_async(&self) -> Option<&dyn super::AsyncDbConnection<Arc<SnowflakeApi>, &'a (dyn Sync)>> {
        Some(self)
    }
}

#[async_trait]
impl<'a> AsyncDbConnection<Arc<SnowflakeApi>, &'a (dyn Sync)> for SnowflakeConnection {
    fn new(api: Arc<SnowflakeApi>) -> Self {
        SnowflakeConnection { api }
    }

    async fn get_schema(
        &self,
        table_reference: &TableReference,
    ) -> Result<SchemaRef, super::Error> {
        let table = table_reference.to_quoted_string();

        let res = self
            .api
            .exec(format!("SELECT * FROM {table} limit 1").as_str())
            .await
            .boxed()
            .context(super::UnableToGetSchemaSnafu)?;

        match res {
            snowflake_api::QueryResult::Arrow(record_batches) => {
                let record_batch = snowflake_schema_cast(&record_batches[0])
                    .boxed()
                    .context(super::UnableToGetSchemaSnafu)?;
                let schema = record_batch.schema();
                return Ok(Arc::clone(&schema));
            }
            snowflake_api::QueryResult::Empty => Err(super::Error::UnableToGetSchema {
                source: "Empty response".to_string().into(),
            }),
            snowflake_api::QueryResult::Json(_json) => Err(super::Error::UnableToGetSchema {
                source: "Unexpected response".to_string().into(),
            }),
        }
    }

    async fn query_arrow(
        &self,
        sql: &str,
        _: &[&'a (dyn Sync)],
    ) -> Result<SendableRecordBatchStream> {
        let sql = sql.to_string();

        let stream = self
            .api
            .exec_streamed(&sql)
            .await
            .context(SnowflakeQuerySnafu)?;

        let mut transformed_stream = stream.map(|batch| {
            batch.and_then(|batch| {
                snowflake_schema_cast(&batch)
                    .map_err(|e| arrow::error::ArrowError::ExternalError(Box::new(e)))
            })
        });

        let Some(first_batch) = transformed_stream.next().await else {
            return Ok(Box::pin(RecordBatchStreamAdapter::new(
                Arc::new(Schema::empty()),
                stream::empty(),
            )));
        };

        let batch = first_batch.context(SnowflakeArrowSnafu)?;

        let schema = batch.schema();

        // add first batch back to stream
        let run_once = stream::once(async move { Ok(batch) });
        let stream_adapter = RecordBatchStreamAdapter::new(
            schema,
            Box::pin(
                run_once
                    .chain(transformed_stream)
                    .map_err(to_execution_error),
            ),
        );

        return Ok(Box::pin(stream_adapter));
    }

    async fn execute(&self, _query: &str, _: &[&'a (dyn Sync)]) -> Result<u64> {
        return NotImplementedSnafu.fail()?;
    }
}

fn to_execution_error(e: impl Into<Box<dyn std::error::Error>>) -> DataFusionError {
    DataFusionError::Execution(format!("{}", e.into()).to_string())
}

/// Converts `Snowflake` specific types to standard Arrow types.
///
/// # Errors
///
/// Returns an error if there is a failure in converting Snowflake to Arrow types.
pub fn snowflake_schema_cast(record_batch: &RecordBatch) -> Result<RecordBatch, Error> {
    let mut fields = Vec::new();
    let mut columns = Vec::new();

    for (idx, field) in record_batch.schema().fields().iter().enumerate() {
        let column = record_batch.column(idx);
        if let Some(sf_logical_type) = field.metadata().get("logicalType") {
            if sf_logical_type.to_lowercase().as_str() == "timestamp_ntz" {
                fields.push(Arc::new(Field::new(
                    field.name(),
                    DataType::Timestamp(TimeUnit::Millisecond, None),
                    field.is_nullable(),
                )));
                columns.push(cast_sf_timestamp_ntz_to_arrow_timestamp(column)?);
                continue;
            }
        }
        fields.push(Arc::clone(field));
        columns.push(Arc::clone(column));
    }

    let schema = Arc::new(Schema::new(fields));
    RecordBatch::try_new(schema, columns).context(FailedToCreateRecordBatchSnafu)
}

fn cast_sf_timestamp_ntz_to_arrow_timestamp(column: &ArrayRef) -> Result<ArrayRef, Error> {
    let struct_array = column.as_any().downcast_ref::<StructArray>().context(
        UnableToCastSnowflakeTimestampSnafu {
            reason: "value is not a struct",
        },
    )?;
    if struct_array.columns().len() < 2 {
        return UnableToCastSnowflakeTimestampSnafu {
            reason: "value is not a struct with 2 columns",
        }
        .fail();
    }
    let epoch_array = struct_array
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .context(UnableToCastSnowflakeTimestampSnafu {
            reason: "epoch is missing",
        })?;
    let fraction_array = struct_array
        .column(1)
        .as_any()
        .downcast_ref::<Int32Array>()
        .context(UnableToCastSnowflakeTimestampSnafu {
            reason: "fraction is missing",
        })?;

    let mut builder = TimestampMillisecondBuilder::new();

    for idx in 0..struct_array.len() {
        if struct_array.is_null(idx) {
            builder.append_null();
        } else {
            let epoch = epoch_array.value(idx);
            let fraction = i64::from(fraction_array.value(idx));
            let timestamp = epoch * 1_000 + fraction / 1_000_000;
            builder.append_value(timestamp);
        }
    }
    Ok(Arc::new(builder.finish()) as ArrayRef)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{
        ArrayBuilder, ArrayRef, Int32Builder, Int64Builder, StructBuilder,
        TimestampMillisecondArray,
    };
    use arrow::datatypes::{DataType, Field};
    use std::sync::Arc;

    #[test]
    fn test_cast_sf_timestamp_ntz_to_arrow_timestamp() {
        let timestamp_ntz_array = create_timestamp_ntz_array(
            vec![Some(1_696_164_330), None, Some(1_714_647_301)],
            vec![Some(0), None, Some(739_000_000)],
        );
        let result = cast_sf_timestamp_ntz_to_arrow_timestamp(&timestamp_ntz_array)
            .expect("Should cast Snowflake timestamp to Arrow timestamp");
        let result = result
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("Should downcast to TimestampMillisecondArray");

        let expected_timestamps = [Some(1_696_164_330_000), None, Some(1_714_647_301_739)];

        assert_eq!(result.value(0), expected_timestamps[0].unwrap_or_default());
        assert!(result.is_null(1));
        assert_eq!(result.value(2), expected_timestamps[2].unwrap_or_default());
    }

    #[test]
    fn test_cast_sf_timestamp_ntz_to_arrow_timestamp_invalid_input() {
        let epoch_array = Arc::new(Int64Array::from(vec![
            Some(1_696_164_330),
            None,
            Some(1_714_647_301),
        ])) as ArrayRef;

        let timestamp_ntz_no_fraction = StructArray::from(vec![(
            Arc::new(Field::new("epoch", DataType::Int64, true)),
            epoch_array,
        )]);

        let result = cast_sf_timestamp_ntz_to_arrow_timestamp(
            &(Arc::new(timestamp_ntz_no_fraction) as ArrayRef),
        );

        assert!(result.is_err());
    }

    fn create_timestamp_ntz_array(
        epochs: Vec<Option<i64>>,
        fractions: Vec<Option<i32>>,
    ) -> ArrayRef {
        let fields = vec![
            Field::new("epoch", DataType::Int64, true),
            Field::new("fraction", DataType::Int32, true),
        ];

        let mut builder = StructBuilder::new(
            fields.clone(),
            vec![
                Box::new(Int64Builder::new()) as Box<dyn ArrayBuilder>,
                Box::new(Int32Builder::new()) as Box<dyn ArrayBuilder>,
            ],
        );

        for (epoch, fraction) in epochs.into_iter().zip(fractions.into_iter()) {
            if let (Some(epoch_val), Some(fraction_val)) = (epoch, fraction) {
                builder
                    .field_builder::<Int64Builder>(0)
                    .expect("Should return a field builder")
                    .append_value(epoch_val);
                builder
                    .field_builder::<Int32Builder>(1)
                    .expect("Should return a field builder")
                    .append_value(fraction_val);
                builder.append(true);
            } else {
                builder.append(false);
                builder
                    .field_builder::<Int64Builder>(0)
                    .expect("Should return a field builder")
                    .append_null();
                builder
                    .field_builder::<Int32Builder>(1)
                    .expect("Should return a field builder")
                    .append_null();
            }
        }

        Arc::new(builder.finish()) as ArrayRef
    }
}

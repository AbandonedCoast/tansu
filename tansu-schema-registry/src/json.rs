// Copyright ⓒ 2024-2025 Peter Morgan <peter.james.morgan@gmail.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::{any::type_name_of_val, sync::Arc};

use crate::{AsArrow, AsJsonValue, AsKafkaRecord, Error, Result, Validator};
use bytes::Bytes;
use datafusion::arrow::{
    array::{
        ArrayBuilder, BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, NullBuilder,
        StringBuilder, StructBuilder, UInt64Builder,
    },
    datatypes::{DataType, Field, Fields},
    record_batch::RecordBatch,
};
use serde_json::{Map, Value};
use tansu_kafka_sans_io::{ErrorCode, record::inflated::Batch};
use tracing::{debug, error, warn};

#[derive(Debug, Default)]
pub struct Schema {
    key: Option<jsonschema::Validator>,
    value: Option<jsonschema::Validator>,
}

fn validate(validator: Option<&jsonschema::Validator>, encoded: Option<Bytes>) -> Result<()> {
    debug!(validator = ?validator, ?encoded);

    validator
        .map_or(Ok(()), |validator| {
            encoded.map_or(Err(Error::Api(ErrorCode::InvalidRecord)), |encoded| {
                serde_json::from_reader(&encoded[..])
                    .map_err(|err| {
                        warn!(?err, ?encoded);
                        Error::Api(ErrorCode::InvalidRecord)
                    })
                    .inspect(|instance| debug!(?instance))
                    .and_then(|instance| {
                        validator
                            .validate(&instance)
                            .inspect_err(|err| warn!(?err, ?validator, %instance))
                            .map_err(|_err| Error::Api(ErrorCode::InvalidRecord))
                    })
            })
        })
        .inspect(|r| debug!(?r))
        .inspect_err(|err| warn!(?err))
}

impl TryFrom<Bytes> for Schema {
    type Error = Error;

    fn try_from(encoded: Bytes) -> Result<Self, Self::Error> {
        serde_json::from_slice::<Value>(&encoded[..])
            .map_err(Into::into)
            .map(|schema| {
                schema.get("properties").map_or(
                    Self {
                        key: None,
                        value: None,
                    },
                    |properties| Self {
                        key: properties
                            .get("key")
                            .and_then(|schema| jsonschema::validator_for(schema).ok()),

                        value: properties
                            .get("value")
                            .and_then(|schema| jsonschema::validator_for(schema).ok()),
                    },
                )
            })
    }
}

impl Validator for Schema {
    fn validate(&self, batch: &Batch) -> Result<()> {
        debug!(?batch);

        for record in &batch.records {
            debug!(?record);

            validate(self.key.as_ref(), record.key.clone())
                .and(validate(self.value.as_ref(), record.value.clone()))?
        }

        Ok(())
    }
}

fn sort_dedup(mut input: Vec<DataType>) -> Vec<DataType> {
    input.sort();
    input.dedup();
    input
}

struct Record {
    key: Option<Value>,
    value: Option<Value>,
}

fn data_type_builder(data_type: &DataType) -> Box<dyn ArrayBuilder> {
    match data_type {
        DataType::Null => Box::new(NullBuilder::new()),
        DataType::Boolean => Box::new(BooleanBuilder::new()),
        DataType::UInt64 => Box::new(UInt64Builder::new()),
        DataType::Int64 => Box::new(Int64Builder::new()),
        DataType::Float64 => Box::new(Float64Builder::new()),
        DataType::Utf8 => Box::new(StringBuilder::new()),
        DataType::List(element) => {
            Box::new(ListBuilder::new(data_type_builder(element.data_type())))
        }
        DataType::Struct(fields) => Box::new(StructBuilder::new(
            fields.to_owned(),
            fields
                .iter()
                .map(|field| data_type_builder(field.data_type()))
                .collect::<Vec<_>>(),
        )),

        _ => unimplemented!("unexpected: {}", type_name_of_val(data_type)),
    }
}

fn common_data_type(values: &[Value]) -> Result<DataType> {
    values
        .iter()
        .map(data_type)
        .collect::<Result<Vec<_>>>()
        .map(sort_dedup)
        .and_then(|mut data_types| {
            if data_types.len() > 1 {
                Err(Error::NoCommonType(data_types))
            } else if let Some(data_type) = data_types.pop() {
                Ok(data_type)
            } else {
                Ok(DataType::Null)
            }
        })
        .inspect(|data_type| debug!(?values, ?data_type))
        .inspect_err(|err| error!(?err, ?values))
}

fn data_type(value: &Value) -> Result<DataType> {
    match value {
        Value::Null => Ok(DataType::Null),
        Value::Bool(_) => Ok(DataType::Boolean),
        Value::Number(value) => {
            if value.is_u64() {
                Ok(DataType::UInt64)
            } else if value.is_i64() {
                Ok(DataType::Int64)
            } else {
                Ok(DataType::Float64)
            }
        }
        Value::String(_) => Ok(DataType::Utf8),
        Value::Array(values) => {
            common_data_type(values).map(|data_type| DataType::new_list(data_type, true))
        }
        Value::Object(object) => object
            .iter()
            .map(|(k, v)| data_type(v).map(|data_type| Field::new(k.to_owned(), data_type, true)))
            .collect::<Result<Vec<_>>>()
            .map(Fields::from)
            .map(DataType::Struct),
    }
    .inspect(|data_type| debug!(?value, ?data_type))
    .inspect_err(|err| error!(?err, ?value))
}

fn append_list_builder(
    element: Arc<Field>,
    items: Vec<Value>,
    builder: &mut ListBuilder<Box<dyn ArrayBuilder>>,
) -> Result<()> {
    let values = builder.values().as_any_mut();

    for item in items {
        match (element.data_type(), item) {
            (_, Value::Null) => values
                .downcast_mut::<NullBuilder>()
                .ok_or(Error::Downcast)
                .map(|builder| builder.append_null())?,

            (_, Value::Bool(value)) => values
                .downcast_mut::<BooleanBuilder>()
                .ok_or(Error::Downcast)
                .map(|builder| builder.append_value(value))?,

            (DataType::UInt64, Value::Number(value)) if value.is_u64() => values
                .downcast_mut::<UInt64Builder>()
                .ok_or(Error::Downcast)
                .map(|builder| {
                    if let Some(value) = value.as_u64() {
                        builder.append_value(value)
                    } else {
                        builder.append_null()
                    }
                })
                .inspect_err(|err| error!(?value, ?err))?,

            (DataType::Int64, Value::Number(value)) if value.is_i64() => values
                .downcast_mut::<Int64Builder>()
                .ok_or(Error::Downcast)
                .map(|builder| {
                    if let Some(value) = value.as_i64() {
                        builder.append_value(value)
                    } else {
                        builder.append_null()
                    }
                })
                .inspect_err(|err| error!(?value, ?err))?,

            (DataType::Float64, Value::Number(value)) if value.is_f64() => values
                .downcast_mut::<Float64Builder>()
                .ok_or(Error::Downcast)
                .map(|builder| {
                    if let Some(value) = value.as_f64() {
                        builder.append_value(value)
                    } else {
                        builder.append_null()
                    }
                })
                .inspect_err(|err| error!(?value, ?err))?,

            (_, Value::String(value)) => values
                .downcast_mut::<StringBuilder>()
                .ok_or(Error::Downcast)
                .map(|builder| builder.append_value(value))?,

            (DataType::List(element), Value::Array(items)) => values
                .downcast_mut::<ListBuilder<Box<dyn ArrayBuilder>>>()
                .ok_or(Error::Downcast)
                .inspect_err(|err| error!(?err, ?element, ?items))
                .and_then(|builder| append_list_builder(element.to_owned(), items, builder))?,

            (DataType::Struct(fields), Value::Object(object)) => values
                .downcast_mut::<StructBuilder>()
                .ok_or(Error::Downcast)
                .inspect_err(|err| error!(?err, ?fields, ?object))
                .and_then(|builder| append_struct_builder(fields, object, builder))?,

            (data_type, value) => Err(Error::UnsupportedSchemaRuntimeValue(
                data_type.to_owned(),
                value,
            ))?,
        }
    }

    builder.append(true);

    Ok(())
}

fn append_struct_builder(
    fields: &Fields,
    mut object: Map<String, Value>,
    builder: &mut StructBuilder,
) -> Result<()> {
    debug!(?fields, ?object);

    for (index, field) in fields.iter().enumerate() {
        if let Some(value) = object.remove(field.name()) {
            match (field.data_type(), value) {
                (_, Value::Null) => builder
                    .field_builder::<NullBuilder>(index)
                    .ok_or(Error::Downcast)
                    .map(|builder| builder.append_null())
                    .inspect_err(|err| error!(?err))?,

                (_, Value::Bool(value)) => builder
                    .field_builder::<BooleanBuilder>(index)
                    .ok_or(Error::Downcast)
                    .map(|builder| builder.append_value(value))
                    .inspect_err(|err| error!(?err))?,

                (DataType::UInt64, Value::Number(value)) if value.is_u64() => builder
                    .field_builder::<UInt64Builder>(index)
                    .ok_or(Error::Downcast)
                    .map(|builder| {
                        if let Some(value) = value.as_u64() {
                            builder.append_value(value)
                        } else {
                            builder.append_null()
                        }
                    })
                    .inspect_err(|err| error!(?field, ?value, ?err))?,

                (DataType::Int64, Value::Number(value)) if value.is_i64() => builder
                    .field_builder::<Int64Builder>(index)
                    .ok_or(Error::Downcast)
                    .map(|builder| {
                        if let Some(value) = value.as_i64() {
                            builder.append_value(value)
                        } else {
                            builder.append_null()
                        }
                    })?,

                (DataType::Float64, Value::Number(value)) if value.is_f64() => builder
                    .field_builder::<Float64Builder>(index)
                    .ok_or(Error::Downcast)
                    .map(|builder| {
                        if let Some(value) = value.as_f64() {
                            builder.append_value(value)
                        } else {
                            builder.append_null()
                        }
                    })?,

                (DataType::Utf8, Value::String(value)) => builder
                    .field_builder::<StringBuilder>(index)
                    .ok_or(Error::Downcast)
                    .map(|builder| builder.append_value(value))
                    .inspect_err(|err| error!(?err))?,

                (DataType::List(element), Value::Array(items)) => builder
                    .field_builder::<ListBuilder<Box<dyn ArrayBuilder>>>(index)
                    .ok_or(Error::Downcast)
                    .and_then(|builder| append_list_builder(element.to_owned(), items, builder))
                    .inspect_err(|err| error!(?err))?,

                (DataType::Struct(fields), Value::Object(object)) => builder
                    .field_builder::<StructBuilder>(index)
                    .ok_or(Error::Downcast)
                    .and_then(|builder| append_struct_builder(fields, object, builder))
                    .inspect_err(|err| error!(?err))?,

                (data_type, value) => Err(Error::UnsupportedSchemaRuntimeValue(
                    data_type.to_owned(),
                    value,
                ))?,
            }
        }
    }

    builder.append(true);

    Ok(())
}

fn append(field: &Field, value: Value, builder: &mut dyn ArrayBuilder) -> Result<()> {
    debug!(?field, ?value, builder = type_name_of_val(builder));

    match (field.data_type(), value) {
        (DataType::Null, _) => builder
            .as_any_mut()
            .downcast_mut::<NullBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (DataType::Boolean, Value::Bool(value)) => builder
            .as_any_mut()
            .downcast_mut::<BooleanBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (DataType::UInt64, Value::Number(value)) if value.is_u64() => builder
            .as_any_mut()
            .downcast_mut::<UInt64Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| {
                if let Some(value) = value.as_u64() {
                    builder.append_value(value)
                } else {
                    builder.append_null()
                }
            })
            .inspect_err(|err| error!(?field, ?value, ?err)),

        (DataType::Int64, Value::Number(value)) if value.is_i64() => builder
            .as_any_mut()
            .downcast_mut::<Int64Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| {
                if let Some(value) = value.as_i64() {
                    builder.append_value(value)
                } else {
                    builder.append_null()
                }
            }),

        (DataType::Float64, Value::Number(value)) if value.is_f64() => builder
            .as_any_mut()
            .downcast_mut::<Float64Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| {
                if let Some(value) = value.as_f64() {
                    builder.append_value(value)
                } else {
                    builder.append_null()
                }
            }),

        (DataType::Utf8, Value::String(value)) => builder
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (DataType::List(element), Value::Array(items)) => builder
            .as_any_mut()
            .downcast_mut::<ListBuilder<Box<dyn ArrayBuilder>>>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?element, ?items))
            .and_then(|builder| append_list_builder(element.to_owned(), items, builder)),

        (DataType::Struct(fields), Value::Object(object)) => builder
            .as_any_mut()
            .downcast_mut::<StructBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?fields, ?object))
            .and_then(|builder| append_struct_builder(fields, object, builder)),

        (data_type, value) => Err(Error::UnsupportedSchemaRuntimeValue(
            data_type.to_owned(),
            value,
        ))?,
    }
}

impl AsArrow for Schema {
    fn as_arrow(&self, batch: &Batch) -> Result<RecordBatch> {
        debug!(?batch);

        let mut builders = vec![];
        let mut fields = vec![];

        if let Some(data_type) = batch
            .records
            .iter()
            .map(|record| {
                record.key.clone().map_or(Ok(None), |encoded| {
                    serde_json::from_slice::<Value>(&encoded[..])
                        .map(Some)
                        .map_err(Into::into)
                })
            })
            .collect::<Result<Vec<_>>>()
            .map(|values| values.into_iter().flatten().collect::<Vec<_>>())
            .and_then(|values| {
                if values.is_empty() {
                    Ok(None)
                } else {
                    common_data_type(values.as_slice()).map(Some)
                }
            })
            .inspect(|data_type| debug!(?data_type))?
        {
            builders.push(data_type_builder(&data_type));
            fields.push(Field::new("key", data_type, true))
        };

        if let Some(data_type) = batch
            .records
            .iter()
            .map(|record| {
                record.value.clone().map_or(Ok(None), |encoded| {
                    serde_json::from_slice::<Value>(&encoded[..])
                        .map(Some)
                        .map_err(Into::into)
                })
            })
            .collect::<Result<Vec<_>>>()
            .map(|values| values.into_iter().flatten().collect::<Vec<_>>())
            .and_then(|values| {
                if values.is_empty() {
                    Ok(None)
                } else {
                    common_data_type(values.as_slice()).map(Some)
                }
            })
            .inspect(|data_type| debug!(?data_type))?
        {
            builders.push(data_type_builder(&data_type));
            fields.push(Field::new("value", data_type, true))
        };

        for kv in batch
            .records
            .iter()
            .map(|record| {
                record
                    .key
                    .as_ref()
                    .map(|encoded| serde_json::from_slice::<Value>(&encoded[..]))
                    .transpose()
                    .map_err(Into::into)
                    .and_then(|key| {
                        record
                            .value
                            .as_ref()
                            .map(|encoded| serde_json::from_slice::<Value>(&encoded[..]))
                            .transpose()
                            .map_err(Into::into)
                            .map(|value| Record { key, value })
                    })
            })
            .collect::<Result<Vec<_>>>()?
        {
            let mut i = fields.iter().zip(builders.iter_mut());

            if let Some(value) = kv.key {
                let (field, builder) = i.next().unwrap();
                debug!(?value, ?field);
                append(field, value, builder)?;
            }

            if let Some(value) = kv.value {
                let (field, builder) = i.next().unwrap();
                debug!(?value, ?field);
                append(field, value, builder)?;
            }
        }

        RecordBatch::try_new(
            Arc::new(datafusion::arrow::datatypes::Schema::new(Fields::from(
                fields,
            ))),
            builders
                .iter_mut()
                .map(|builder| builder.finish())
                .collect(),
        )
        .map_err(Into::into)
    }
}

impl AsKafkaRecord for Schema {
    fn as_kafka_record(&self, value: &Value) -> Result<tansu_kafka_sans_io::record::Builder> {
        let mut builder = tansu_kafka_sans_io::record::Record::builder();

        if let Some(value) = value.get("key") {
            debug!(?value);

            if self.key.is_some() {
                builder = builder.key(serde_json::to_vec(value).map(Bytes::from).map(Into::into)?);
            }
        }

        if let Some(value) = value.get("value") {
            debug!(?value);

            if self.value.is_some() {
                builder =
                    builder.value(serde_json::to_vec(value).map(Bytes::from).map(Into::into)?);
            }
        }

        Ok(builder)
    }
}

impl AsJsonValue for Schema {
    fn as_json_value(&self, batch: &Batch) -> Result<Value> {
        let _ = batch;
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use crate::Registry;

    use super::*;
    use datafusion::{arrow::util::pretty::pretty_format_batches, prelude::*};
    use jsonschema::BasicOutput;
    use object_store::{ObjectStore, PutPayload, memory::InMemory, path::Path};
    use serde_json::json;
    use std::{collections::VecDeque, fs::File, ops::Deref, sync::Arc, thread};
    use tansu_kafka_sans_io::record::Record;
    use tracing::subscriber::DefaultGuard;
    use tracing_subscriber::EnvFilter;

    fn init_tracing() -> Result<DefaultGuard> {
        Ok(tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_level(true)
                .with_line_number(true)
                .with_thread_names(false)
                .with_env_filter(
                    EnvFilter::from_default_env()
                        .add_directive(format!("{}=debug", env!("CARGO_CRATE_NAME")).parse()?),
                )
                .with_writer(
                    thread::current()
                        .name()
                        .ok_or(Error::Message(String::from("unnamed thread")))
                        .and_then(|name| {
                            File::create(format!("../logs/{}/{name}.log", env!("CARGO_PKG_NAME"),))
                                .map_err(Into::into)
                        })
                        .map(Arc::new)?,
                )
                .finish(),
        ))
    }

    #[tokio::test]
    async fn key_only_invalid_record() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                        },
                        "email": {
                            "type": "string",
                            "format": "email"
                        }
                    }
                }
            }
        }))
        .map(Bytes::from)
        .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let key = serde_json::to_vec(&json!(12320)).map(Bytes::from)?;

        let batch = Batch::builder()
            .record(Record::builder().key(key.clone().into()))
            .build()?;

        assert!(matches!(
            registry.validate(topic, &batch).await,
            Err(Error::Api(ErrorCode::InvalidRecord))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn value_only_invalid_record() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                        },
                        "email": {
                            "type": "string",
                            "format": "email"
                        }
                    }
                }
            }
        }))
        .map(Bytes::from)
        .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let value = serde_json::to_vec(&json!({
            "name": "alice",
            "email": "alice@example.com"}))
        .map(Bytes::from)?;

        let batch = Batch::builder()
            .record(Record::builder().value(value.clone().into()))
            .build()?;

        assert!(matches!(
            registry.validate(topic, &batch).await,
            Err(Error::Api(ErrorCode::InvalidRecord))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn key_and_value() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                        },
                        "email": {
                            "type": "string",
                            "format": "email"
                        }
                    }
                }
            }
        }))
        .map(Bytes::from)
        .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let key = serde_json::to_vec(&json!(12320)).map(Bytes::from)?;

        let value = serde_json::to_vec(&json!({
                "name": "alice",
                "email": "alice@example.com"}))
        .map(Bytes::from)?;

        let batch = Batch::builder()
            .record(
                Record::builder()
                    .key(key.clone().into())
                    .value(value.clone().into()),
            )
            .build()?;

        registry.validate(topic, &batch).await
    }

    #[tokio::test]
    async fn no_schema() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let registry = Registry::new(InMemory::new());

        let key = Bytes::from_static(b"Lorem ipsum dolor sit amet");
        let value = Bytes::from_static(b"Consectetur adipiscing elit");

        let batch = Batch::builder()
            .record(
                Record::builder()
                    .key(key.clone().into())
                    .value(value.clone().into()),
            )
            .build()?;

        registry.validate(topic, &batch).await
    }

    #[tokio::test]
    async fn empty_schema() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({}))
            .map(Bytes::from)
            .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let key = Bytes::from_static(b"Lorem ipsum dolor sit amet");
        let value = Bytes::from_static(b"Consectetur adipiscing elit");

        let batch = Batch::builder()
            .record(
                Record::builder()
                    .key(key.clone().into())
                    .value(value.clone().into()),
            )
            .build()?;

        registry.validate(topic, &batch).await
    }

    #[tokio::test]
    async fn key_schema_only() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
            }
        }))
        .map(Bytes::from)
        .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let key = serde_json::to_vec(&json!(12320)).map(Bytes::from)?;

        let value = Bytes::from_static(b"Consectetur adipiscing elit");

        let batch = Batch::builder()
            .record(
                Record::builder()
                    .key(key.clone().into())
                    .value(value.clone().into()),
            )
            .build()?;

        registry.validate(topic, &batch).await
    }

    #[tokio::test]
    async fn bad_key() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
            }
        }))
        .map(Bytes::from)
        .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let key = Bytes::from_static(b"Lorem ipsum dolor sit amet");

        let batch = Batch::builder()
            .record(Record::builder().key(key.clone().into()))
            .build()?;

        assert!(matches!(
            registry.validate(topic, &batch).await,
            Err(Error::Api(ErrorCode::InvalidRecord))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn value_schema_only() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                        },
                        "email": {
                            "type": "string",
                            "format": "email"
                        }
                    }
                }
            }
        }))
        .map(Bytes::from)
        .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let key = Bytes::from_static(b"Lorem ipsum dolor sit amet");

        let value = serde_json::to_vec(&json!({
                    "name": "alice",
                    "email": "alice@example.com"}))
        .map(Bytes::from)?;

        let batch = Batch::builder()
            .record(
                Record::builder()
                    .key(key.clone().into())
                    .value(value.clone().into()),
            )
            .build()?;

        registry.validate(topic, &batch).await
    }

    #[tokio::test]
    async fn bad_value() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let payload = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                        },
                        "email": {
                            "type": "string",
                            "format": "email"
                        }
                    }
                }
            }
        }))
        .map(Bytes::from)
        .map(PutPayload::from)?;

        let object_store = InMemory::new();
        let location = Path::from(format!("{topic}.json"));
        _ = object_store.put(&location, payload).await?;

        let registry = Registry::new(object_store);

        let value = Bytes::from_static(b"Consectetur adipiscing elit");

        let batch = Batch::builder()
            .record(Record::builder().value(value.clone().into()))
            .build()?;

        assert!(matches!(
            registry.validate(topic, &batch).await,
            Err(Error::Api(ErrorCode::InvalidRecord))
        ));

        Ok(())
    }

    #[test]
    fn integer_type_can_be_float_dot_zero() -> Result<()> {
        let schema = json!({"type": "integer"});
        let validator = jsonschema::validator_for(&schema)?;

        assert!(validator.is_valid(&json!(42)));
        assert!(validator.is_valid(&json!(-1)));
        assert!(validator.is_valid(&json!(1.0)));

        Ok(())
    }

    #[test]
    fn array_with_items_type_basic_output() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "array",
                    "items": {
                        "type": "number"
                    }
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        assert!(matches!(
            schema
                .value
                .as_ref()
                .unwrap()
                .apply(&json!([1, 2, 3, 4, 5]))
                .basic(),
            BasicOutput::Valid(_),
        ));

        assert!(matches!(
            schema
                .value
                .as_ref()
                .unwrap()
                .apply(&json!([-1, 2.3, 3, 4.0, 5]))
                .basic(),
            BasicOutput::Valid(_),
        ));

        assert!(matches!(
            schema
                .value
                .as_ref()
                .unwrap()
                .apply(&json!([3, "different", { "types": "of values" }]))
                .basic(),
            BasicOutput::Invalid(_),
        ));

        assert!(matches!(
            schema
                .value
                .as_ref()
                .unwrap()
                .apply(&json!({"Not": "an array"}))
                .basic(),
            BasicOutput::Invalid(_)
        ));

        Ok(())
    }

    #[test]
    fn array_basic_output() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "array",
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        assert_eq!(
            BasicOutput::Valid(VecDeque::new()),
            schema
                .value
                .as_ref()
                .unwrap()
                .apply(&json!([1, 2, 3, 4, 5]))
                .basic()
        );

        assert_eq!(
            BasicOutput::Valid(VecDeque::new()),
            schema
                .value
                .as_ref()
                .unwrap()
                .apply(&json!([3, "different", { "types": "of values" }]))
                .basic()
        );

        assert!(matches!(
            schema
                .value
                .as_ref()
                .unwrap()
                .apply(&json!({"Not": "an array"}))
                .basic(),
            BasicOutput::Invalid(_)
        ));

        Ok(())
    }

    #[test]
    fn schema_basic_output() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                        },
                        "email": {
                            "type": "string",
                            "format": "email"
                        }
                    }
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        debug!(?schema);

        assert_eq!(
            BasicOutput::Valid(VecDeque::new()),
            schema.key.as_ref().unwrap().apply(&json!(12321)).basic()
        );

        match schema
            .value
            .as_ref()
            .unwrap()
            .apply(&json!({"name": "alice", "email": "alice@example.com"}))
            .basic()
        {
            BasicOutput::Valid(annotations) => {
                debug!(?annotations);
                assert_eq!(1, annotations.len());
                assert_eq!(
                    &Value::Array(vec![
                        Value::String("email".into()),
                        Value::String("name".into())
                    ]),
                    annotations[0].value().deref()
                );

                for annotation in annotations {
                    debug!(
                        "value: {} at path {}",
                        annotation.value(),
                        annotation.instance_location()
                    )
                }
            }
            BasicOutput::Invalid(errors) => {
                debug!(?errors);
                panic!()
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn key_and_value_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                        },
                        "email": {
                            "type": "string",
                            "format": "email"
                        }
                    }
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        let kv = [
            (
                json!(12321),
                json!({"name": "alice", "email": "alice@example.com"}),
            ),
            (
                json!(32123),
                json!({"name": "bob", "email": "bob@example.com"}),
            ),
        ];

        let batch = {
            let mut batch = Batch::builder();

            for (ref key, ref value) in kv {
                batch = batch.record(
                    Record::builder()
                        .key(serde_json::to_vec(key).map(Bytes::from).map(Into::into)?)
                        .value(serde_json::to_vec(value).map(Bytes::from).map(Into::into)?),
                );
            }

            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch(topic, record_batch)?;
        let df = ctx.sql(format!("select * from {topic}").as_str()).await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results)?.to_string();

        let expected = vec![
            "+-------+-----------------------------------------+",
            "| key   | value                                   |",
            "+-------+-----------------------------------------+",
            "| 12321 | {email: alice@example.com, name: alice} |",
            "| 32123 | {email: bob@example.com, name: bob}     |",
            "+-------+-----------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn key_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        let keys = [json!(12321), json!(23432), json!(34543)];

        let batch = {
            let mut batch = Batch::builder();

            for ref key in keys {
                batch = batch.record(
                    Record::builder()
                        .key(serde_json::to_vec(key).map(Bytes::from).map(Into::into)?),
                );
            }

            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch(topic, record_batch)?;
        let df = ctx.sql(format!("select * from {topic}").as_str()).await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results)?.to_string();

        let expected = vec![
            "+-------+",
            "| key   |",
            "+-------+",
            "| 12321 |",
            "| 23432 |",
            "| 34543 |",
            "+-------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn primitive_key_and_value_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "string",
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        let kv = [
            (json!(12321), json!("alice@example.com")),
            (json!(32123), json!("bob@example.com")),
        ];

        let batch = {
            let mut batch = Batch::builder();

            for (ref key, ref value) in kv {
                batch = batch.record(
                    Record::builder()
                        .key(serde_json::to_vec(key).map(Bytes::from).map(Into::into)?)
                        .value(serde_json::to_vec(value).map(Bytes::from).map(Into::into)?),
                );
            }

            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch(topic, record_batch)?;
        let df = ctx.sql(format!("select * from {topic}").as_str()).await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results)?.to_string();

        let expected = vec![
            "+-------+-------------------+",
            "| key   | value             |",
            "+-------+-------------------+",
            "| 12321 | alice@example.com |",
            "| 32123 | bob@example.com   |",
            "+-------+-------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn primitive_key_and_array_value_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "array",
                    "items": {
                        "type": "string"
                    }
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        let kv = [
            (json!(12321), json!(["a", "b", "c"])),
            (json!(32123), json!(["p", "q", "r"])),
        ];

        let batch = {
            let mut batch = Batch::builder();

            for (ref key, ref value) in kv {
                batch = batch.record(
                    Record::builder()
                        .key(serde_json::to_vec(key).map(Bytes::from).map(Into::into)?)
                        .value(serde_json::to_vec(value).map(Bytes::from).map(Into::into)?),
                );
            }

            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch(topic, record_batch)?;
        let df = ctx.sql(format!("select * from {topic}").as_str()).await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results)?.to_string();

        let expected = vec![
            "+-------+-----------+",
            "| key   | value     |",
            "+-------+-----------+",
            "| 12321 | [a, b, c] |",
            "| 32123 | [p, q, r] |",
            "+-------+-----------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn primitive_key_and_array_object_value_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "quantity": {
                                "type": "integer",
                            },
                            "location": {
                                "type": "string",
                            }
                        }
                    }
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        let kv = [
            (
                json!(12321),
                json!([{"quantity": 6, "location": "abc"}, {"quantity": 11, "location": "pqr"}]),
            ),
            (
                json!(32123),
                json!([{"quantity": 3, "location": "abc"},
                       {"quantity": 33, "location": "def"},
                       {"quantity": 21, "location": "xyz"}]),
            ),
        ];

        let batch = {
            let mut batch = Batch::builder();

            for (ref key, ref value) in kv {
                batch = batch.record(
                    Record::builder()
                        .key(serde_json::to_vec(key).map(Bytes::from).map(Into::into)?)
                        .value(serde_json::to_vec(value).map(Bytes::from).map(Into::into)?),
                );
            }

            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch(topic, record_batch)?;
        let df = ctx.sql(format!("select * from {topic}").as_str()).await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results)?.to_string();

        let expected = vec![
            "+-------+----------------------------------------------------------------------------------------------+",
            "| key   | value                                                                                        |",
            "+-------+----------------------------------------------------------------------------------------------+",
            "| 12321 | [{location: abc, quantity: 6}, {location: pqr, quantity: 11}]                                |",
            "| 32123 | [{location: abc, quantity: 3}, {location: def, quantity: 33}, {location: xyz, quantity: 21}] |",
            "+-------+----------------------------------------------------------------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn primitive_key_and_struct_with_array_field_value_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let schema = serde_json::to_vec(&json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "number"
                },
                "value": {
                    "type": "object",
                    "properties": {
                        "zone": {
                            "type": "number",
                        },
                        "locations": {
                            "type": "array",
                            "items": {
                                "type": "string"
                            }
                        }
                    }
                }
            }
        }))
        .map_err(Into::into)
        .map(Bytes::from)
        .and_then(Schema::try_from)?;

        let kv = [
            (
                json!(12321),
                json!({"zone": 6, "locations": ["abc", "def"]}),
            ),
            (json!(32123), json!({"zone": 11, "locations": ["pqr"]})),
        ];

        let batch = {
            let mut batch = Batch::builder();

            for (ref key, ref value) in kv {
                batch = batch.record(
                    Record::builder()
                        .key(serde_json::to_vec(key).map(Bytes::from).map(Into::into)?)
                        .value(serde_json::to_vec(value).map(Bytes::from).map(Into::into)?),
                );
            }

            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch(topic, record_batch)?;
        let df = ctx.sql(format!("select * from {topic}").as_str()).await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results)?.to_string();

        let expected = vec![
            "+-------+----------------------------------+",
            "| key   | value                            |",
            "+-------+----------------------------------+",
            "| 12321 | {locations: [abc, def], zone: 6} |",
            "| 32123 | {locations: [pqr], zone: 11}     |",
            "+-------+----------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }
}

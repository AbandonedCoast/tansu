// Copyright ⓒ 2025 Peter Morgan <peter.james.morgan@gmail.com>
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

use std::{collections::HashMap, iter::zip};

use apache_avro::{
    Reader,
    schema::{ArraySchema, MapSchema, RecordSchema, Schema as AvroSchema, UnionSchema},
    types::Value,
};
use bytes::Bytes;
use chrono::NaiveDateTime;
use datafusion::arrow::{
    array::{
        ArrayBuilder, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder,
        Decimal256Builder, Float32Builder, Float64Builder, Int32Builder, Int64Builder, ListBuilder,
        MapBuilder, NullBuilder, StringBuilder, StringDictionaryBuilder, StructBuilder,
        Time32MillisecondBuilder, Time64MicrosecondBuilder, Time64NanosecondBuilder,
        TimestampMicrosecondBuilder, TimestampMillisecondBuilder, TimestampNanosecondBuilder,
        UInt32Builder,
    },
    datatypes::{DataType, Field, FieldRef, Fields, TimeUnit, UInt32Type, UnionFields, UnionMode},
    record_batch::RecordBatch,
};
use num_bigint::BigInt;
use serde_json::{Map, Number, Value as JsonValue};
use tansu_kafka_sans_io::{ErrorCode, record::inflated::Batch};
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::{AsArrow, AsJsonValue, AsKafkaRecord, Error, Result, Validator, arrow::RecordBuilder};

const NULLABLE: bool = true;
const SORTED_MAP_KEYS: bool = false;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schema {
    key: Option<AvroSchema>,
    value: Option<AvroSchema>,
}

impl TryFrom<Bytes> for Schema {
    type Error = Error;

    fn try_from(encoded: Bytes) -> Result<Self, Self::Error> {
        serde_json::from_slice::<JsonValue>(&encoded[..])
            .map_err(Into::into)
            .map(|schema| Self::from(&schema))
    }
}

impl From<&JsonValue> for Schema {
    fn from(schema: &JsonValue) -> Self {
        debug!(?schema);

        schema
            .get("fields")
            .inspect(|fields| debug!(?fields))
            .and_then(|fields| fields.as_array())
            .inspect(|fields| debug!(?fields))
            .map_or(
                Self {
                    key: None,
                    value: None,
                },
                |fields| Self {
                    key: fields
                        .iter()
                        .find(|field| field.get("name").is_some_and(|name| name == "key"))
                        .inspect(|value| debug!(?value))
                        .and_then(|schema| {
                            AvroSchema::parse(schema)
                                .inspect_err(|err| error!(?err, ?schema))
                                .ok()
                        }),

                    value: fields
                        .iter()
                        .find(|field| field.get("name").is_some_and(|name| name == "value"))
                        .inspect(|value| debug!(?value))
                        .and_then(|schema| {
                            AvroSchema::parse(schema)
                                .inspect_err(|err| error!(?err, ?schema))
                                .ok()
                        }),
                },
            )
    }
}

trait NullableVariant {
    fn nullable_variant(&self) -> Option<&AvroSchema>;
}

impl NullableVariant for UnionSchema {
    fn nullable_variant(&self) -> Option<&AvroSchema> {
        if self.variants().len() == 2
            && self
                .variants()
                .iter()
                .inspect(|variant| debug!(?variant))
                .any(|schema| matches!(schema, AvroSchema::Null))
        {
            self.variants()
                .iter()
                .find(|schema| !matches!(schema, AvroSchema::Null))
                .inspect(|schema| debug!(?schema))
        } else {
            None
        }
    }
}

fn schema_data_type(schema: &AvroSchema) -> Result<DataType> {
    debug!(?schema);

    match schema {
        AvroSchema::Null => Ok(DataType::Null),
        AvroSchema::Boolean => Ok(DataType::Boolean),
        AvroSchema::Int => Ok(DataType::Int32),
        AvroSchema::Long => Ok(DataType::Int64),
        AvroSchema::Float => Ok(DataType::Float32),
        AvroSchema::Double => Ok(DataType::Float64),
        AvroSchema::Bytes => Ok(DataType::Binary),
        AvroSchema::String => Ok(DataType::Utf8),

        AvroSchema::Array(schema) => schema_data_type(&schema.items)
            .inspect(|item| debug!(?schema, ?item))
            .map(|item| DataType::new_list(item, NULLABLE)),

        AvroSchema::Map(schema) => schema_data_type(&schema.types)
            .inspect(|value| debug!(?schema, ?value))
            .map(|value| {
                DataType::Map(
                    FieldRef::new(Field::new(
                        "entries",
                        DataType::Struct(Fields::from_iter([
                            Field::new("keys", DataType::Utf8, !NULLABLE),
                            Field::new("values", value, NULLABLE),
                        ])),
                        !NULLABLE,
                    )),
                    SORTED_MAP_KEYS,
                )
            }),

        AvroSchema::Union(schema) => {
            debug!(?schema);

            if let Some(schema) = schema.nullable_variant() {
                schema_data_type(schema)
            } else {
                schema
                    .variants()
                    .iter()
                    .enumerate()
                    .map(|(index, variant)| {
                        schema_data_type(variant)
                            .map(|data_type| {
                                Field::new(format!("field{}", index + 1), data_type, NULLABLE)
                            })
                            .inspect(|field| debug!(?field))
                    })
                    .collect::<Result<Vec<_>>>()
                    .inspect(|fields| debug!(?fields))
                    .and_then(|fields| {
                        i8::try_from(schema.variants().len())
                            .map(|type_ids| {
                                UnionFields::new((1..=type_ids).collect::<Vec<_>>(), fields)
                            })
                            .map_err(Into::into)
                    })
                    .inspect(|union_fields| debug!(?union_fields))
                    .map(|fields| DataType::Union(fields, UnionMode::Dense))
            }
        }

        AvroSchema::Record(schema) => {
            debug!(?schema);
            schema
                .fields
                .iter()
                .map(|field| {
                    schema_data_type(&field.schema)
                        .map(|data_type| Field::new(field.name.clone(), data_type, NULLABLE))
                })
                .collect::<Result<Vec<_>>>()
                .map(Fields::from)
                .map(DataType::Struct)
        }

        AvroSchema::Enum(schema) => {
            debug!(?schema);

            Ok(DataType::Dictionary(
                Box::new(DataType::UInt32),
                Box::new(DataType::Utf8),
            ))
        }

        AvroSchema::Fixed(schema) => i32::try_from(schema.size)
            .map(DataType::FixedSizeBinary)
            .map_err(Into::into),

        AvroSchema::Decimal(schema) => u8::try_from(schema.precision)
            .and_then(|precision| {
                i8::try_from(schema.scale).map(|scale| {
                    if precision <= 16 {
                        DataType::Decimal128(precision, scale)
                    } else {
                        DataType::Decimal256(precision, scale)
                    }
                })
            })
            .map_err(Into::into),

        AvroSchema::BigDecimal => todo!(),
        AvroSchema::Uuid => Ok(DataType::Utf8),
        AvroSchema::Date => Ok(DataType::Date32),

        AvroSchema::TimeMillis => Ok(DataType::Time32(TimeUnit::Millisecond)),

        AvroSchema::TimeMicros => Ok(DataType::Time64(TimeUnit::Microsecond)),

        AvroSchema::TimestampMillis => Ok(DataType::Timestamp(TimeUnit::Millisecond, None)),

        AvroSchema::TimestampMicros => Ok(DataType::Timestamp(TimeUnit::Microsecond, None)),

        AvroSchema::TimestampNanos => Ok(DataType::Timestamp(TimeUnit::Nanosecond, None)),

        AvroSchema::LocalTimestampMillis => Ok(DataType::Timestamp(TimeUnit::Millisecond, None)),

        AvroSchema::LocalTimestampMicros => Ok(DataType::Timestamp(TimeUnit::Microsecond, None)),

        AvroSchema::LocalTimestampNanos => Ok(DataType::Timestamp(TimeUnit::Nanosecond, None)),

        AvroSchema::Duration => Ok(DataType::Struct(Fields::from_iter([
            Field::new("month", DataType::UInt32, NULLABLE),
            Field::new("days", DataType::UInt32, NULLABLE),
            Field::new("milliseconds", DataType::UInt32, NULLABLE),
        ]))),

        AvroSchema::Ref { name } => {
            let _ = name;
            todo!();
        }
    }
}

fn schema_array_builder(schema: &AvroSchema) -> Result<Box<dyn ArrayBuilder>> {
    match schema {
        AvroSchema::Null => Ok(Box::new(NullBuilder::new())),
        AvroSchema::Boolean => Ok(Box::new(BooleanBuilder::new())),
        AvroSchema::Int => Ok(Box::new(Int32Builder::new())),
        AvroSchema::Long => Ok(Box::new(Int64Builder::new())),
        AvroSchema::Float => Ok(Box::new(Float32Builder::new())),
        AvroSchema::Double => Ok(Box::new(Float64Builder::new())),
        AvroSchema::Bytes => Ok(Box::new(BinaryBuilder::new())),
        AvroSchema::String => Ok(Box::new(StringBuilder::new())),

        AvroSchema::Array(schema) => schema_array_builder(&schema.items)
            .map(ListBuilder::new)
            .map(|builder| Box::new(builder) as Box<dyn ArrayBuilder>),

        AvroSchema::Map(schema) => schema_array_builder(&schema.types)
            .map(|builder| {
                MapBuilder::new(
                    None,
                    Box::new(StringBuilder::new()) as Box<dyn ArrayBuilder>,
                    builder,
                )
            })
            .map(|builder| Box::new(builder) as Box<dyn ArrayBuilder>),

        AvroSchema::Union(schema) => {
            debug!(?schema);

            if let Some(schema) = schema.nullable_variant() {
                schema_array_builder(schema)
            } else {
                todo!()
            }
        }

        AvroSchema::Record(schema) => schema
            .fields
            .iter()
            .try_fold((vec![], vec![]), |(mut fields, mut builders), field| {
                schema_data_type(&field.schema)
                    .map(|data_type| {
                        fields.push(Field::new(field.name.clone(), data_type, NULLABLE))
                    })
                    .and(schema_array_builder(&field.schema).map(|builder| builders.push(builder)))
                    .map(|()| (fields, builders))
            })
            .map(|(fields, builders)| StructBuilder::new(fields, builders))
            .map(|builder| Box::new(builder) as Box<dyn ArrayBuilder>),

        AvroSchema::Enum(_schema) => Ok(Box::new(StringDictionaryBuilder::<UInt32Type>::new())),

        AvroSchema::Fixed(_schema) => Ok(Box::new(BinaryBuilder::new())),

        AvroSchema::Decimal(schema) => u8::try_from(schema.precision)
            .map(|precision| {
                if precision <= 16 {
                    Box::new(Decimal128Builder::new()) as Box<dyn ArrayBuilder>
                } else {
                    Box::new(Decimal256Builder::new()) as Box<dyn ArrayBuilder>
                }
            })
            .map_err(Into::into),

        AvroSchema::BigDecimal => todo!(),
        AvroSchema::Uuid => Ok(Box::new(StringBuilder::new())),
        AvroSchema::Date => Ok(Box::new(Date32Builder::new())),
        AvroSchema::TimeMillis => Ok(Box::new(Time32MillisecondBuilder::new())),
        AvroSchema::TimeMicros => Ok(Box::new(Time64MicrosecondBuilder::new())),
        AvroSchema::TimestampMillis => Ok(Box::new(TimestampMillisecondBuilder::new())),
        AvroSchema::TimestampMicros => Ok(Box::new(TimestampMicrosecondBuilder::new())),
        AvroSchema::TimestampNanos => Ok(Box::new(TimestampNanosecondBuilder::new())),
        AvroSchema::LocalTimestampMillis => Ok(Box::new(Time32MillisecondBuilder::new())),
        AvroSchema::LocalTimestampMicros => Ok(Box::new(Time64MicrosecondBuilder::new())),
        AvroSchema::LocalTimestampNanos => Ok(Box::new(Time64NanosecondBuilder::new())),

        AvroSchema::Duration => Ok(Box::new(StructBuilder::new(
            vec![
                Field::new("month", DataType::UInt32, NULLABLE),
                Field::new("days", DataType::UInt32, NULLABLE),
                Field::new("milliseconds", DataType::UInt32, NULLABLE),
            ],
            vec![
                Box::new(UInt32Builder::new()),
                Box::new(UInt32Builder::new()),
                Box::new(UInt32Builder::new()),
            ],
        ))),

        AvroSchema::Ref { name } => {
            let _ = name;
            todo!();
        }
    }
}

impl TryFrom<&Schema> for RecordBuilder {
    type Error = Error;

    fn try_from(value: &Schema) -> std::result::Result<Self, Self::Error> {
        let mut keys = vec![];

        if let Some(ref schema) = value.key {
            keys.push(schema_array_builder(schema)?);
        }

        let mut values = vec![];

        if let Some(ref schema) = value.value {
            values.push(schema_array_builder(schema)?)
        }

        Ok(Self { keys, values })
    }
}

macro_rules! try_as {
    ($name:ident, $pattern:path, $type:ty) => {
        fn $name(value: Value) -> Result<$type> {
            if let $pattern(value) = value {
                Ok(value)
            } else {
                Err(Error::InvalidValue(value))
            }
        }
    };
}

try_as!(try_as_i32, Value::Int, i32);
try_as!(try_as_bool, Value::Boolean, bool);
try_as!(try_as_i64, Value::Long, i64);
try_as!(try_as_f32, Value::Float, f32);
try_as!(try_as_f64, Value::Double, f64);
try_as!(try_as_bytes, Value::Bytes, Vec<u8>);
try_as!(try_as_string, Value::String, String);
// try_as!(try_as_map, Value::Map, HashMap<String, Value>);
try_as!(try_as_record, Value::Record, Vec<(String, Value)>);

fn append_list_builder(
    schema: &ArraySchema,
    values: Vec<Value>,
    builder: &mut ListBuilder<Box<dyn ArrayBuilder>>,
) -> Result<()> {
    match schema.items.as_ref() {
        AvroSchema::Null => builder
            .values()
            .as_any_mut()
            .downcast_mut::<NullBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_bool)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| builder.append_nulls(values.len()))
            })?,

        AvroSchema::Boolean => builder
            .values()
            .as_any_mut()
            .downcast_mut::<BooleanBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_bool)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| builder.append_slice(values.as_slice()))
            })?,

        AvroSchema::Int => builder
            .values()
            .as_any_mut()
            .downcast_mut::<Int32Builder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_i32)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| builder.append_slice(values.as_slice()))
            })?,

        AvroSchema::Long => builder
            .values()
            .as_any_mut()
            .downcast_mut::<Int64Builder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_i64)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| builder.append_slice(values.as_slice()))
            })?,

        AvroSchema::Float => builder
            .values()
            .as_any_mut()
            .downcast_mut::<Float32Builder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_f32)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| builder.append_slice(values.as_slice()))
            })?,

        AvroSchema::Double => builder
            .values()
            .as_any_mut()
            .downcast_mut::<Float64Builder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_f64)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| builder.append_slice(values.as_slice()))
            })?,

        AvroSchema::Bytes => builder
            .values()
            .as_any_mut()
            .downcast_mut::<BinaryBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_bytes)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| {
                        for value in values {
                            builder.append_value(value);
                        }
                    })
            })?,

        AvroSchema::String | AvroSchema::Uuid => builder
            .values()
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_string)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| {
                        for value in values {
                            builder.append_value(value);
                        }
                    })
            })?,

        AvroSchema::Array(_schema) => todo!(),
        AvroSchema::Map(_schema) => todo!(),
        AvroSchema::Union(_schema) => todo!(),

        AvroSchema::Record(schema) => builder
            .values()
            .as_any_mut()
            .downcast_mut::<StructBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_record)
                    .collect::<Result<Vec<_>>>()
                    .and_then(|values| {
                        values
                            .into_iter()
                            .map(|items| append_struct_builder(schema, items, builder))
                            .collect::<Result<Vec<_>>>()
                    })
            })
            .map(|_| ())?,

        AvroSchema::Enum(_schema) => todo!(),
        AvroSchema::Fixed(_schema) => todo!(),
        AvroSchema::Decimal(_schema) => todo!(),
        AvroSchema::BigDecimal => todo!(),

        AvroSchema::Date => builder
            .values()
            .as_any_mut()
            .downcast_mut::<Date32Builder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_i32)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| {
                        for value in values {
                            builder.append_value(value);
                        }
                    })
            })?,

        AvroSchema::TimeMillis => builder
            .values()
            .as_any_mut()
            .downcast_mut::<Time32MillisecondBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_i32)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| {
                        for value in values {
                            builder.append_value(value);
                        }
                    })
            })?,

        AvroSchema::TimeMicros => builder
            .values()
            .as_any_mut()
            .downcast_mut::<Time64MicrosecondBuilder>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| {
                values
                    .into_iter()
                    .map(try_as_i64)
                    .collect::<Result<Vec<_>>>()
                    .map(|values| {
                        for value in values {
                            builder.append_value(value);
                        }
                    })
            })?,

        AvroSchema::TimestampMillis => todo!(),
        AvroSchema::TimestampMicros => todo!(),
        AvroSchema::TimestampNanos => todo!(),
        AvroSchema::LocalTimestampMillis => todo!(),
        AvroSchema::LocalTimestampMicros => todo!(),
        AvroSchema::LocalTimestampNanos => todo!(),
        AvroSchema::Duration => todo!(),
        AvroSchema::Ref { name } => {
            let _ = name;
            todo!()
        }
    }

    builder.append(true);

    Ok(())
}

fn append_map_builder(
    schema: &MapSchema,
    values: HashMap<String, Value>,
    builder: &mut MapBuilder<Box<dyn ArrayBuilder>, Box<dyn ArrayBuilder>>,
) -> Result<()> {
    debug!(?schema, ?values);

    for (key, value) in values {
        append_value(None, Value::String(key), builder.keys())?;
        append_value(None, value, builder.values())?;
    }

    builder.append(true).map_err(Into::into)
}

fn append_struct_builder(
    schema: &RecordSchema,
    items: Vec<(String, Value)>,
    builder: &mut StructBuilder,
) -> Result<()> {
    for (index, (field, (name, value))) in zip(schema.fields.as_slice(), items).enumerate() {
        debug!(?index, ?field, ?name, ?value);

        match (&field.schema, value) {
            (AvroSchema::Null, Value::Null) => builder
                .field_builder::<NullBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_null())?,

            (AvroSchema::Boolean, Value::Boolean(value)) => builder
                .field_builder::<BooleanBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::Int, Value::Int(value)) => builder
                .field_builder::<Int32Builder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::Long, Value::Long(value)) => builder
                .field_builder::<Int64Builder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::Float, Value::Float(value)) => builder
                .field_builder::<Float32Builder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::Double, Value::Double(value)) => builder
                .field_builder::<Float64Builder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::Bytes, Value::Bytes(value)) => builder
                .field_builder::<BinaryBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::String, Value::String(value)) => builder
                .field_builder::<StringBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::Array(schema), Value::Array(values)) => builder
                .field_builder::<ListBuilder<Box<dyn ArrayBuilder>>>(index)
                .ok_or(Error::BadDowncast { field: name })
                .inspect_err(|err| error!(?err, ?schema, ?values))
                .and_then(|builder| append_list_builder(schema, values, builder))?,

            (AvroSchema::Map(schema), Value::Map(values)) => builder
                .field_builder::<MapBuilder<Box<dyn ArrayBuilder>, Box<dyn ArrayBuilder>>>(index)
                .ok_or(Error::BadDowncast { field: name })
                .inspect_err(|err| error!(?err, ?schema, ?values))
                .and_then(|builder| append_map_builder(schema, values, builder))?,

            (AvroSchema::Union(_schema), Value::Union(_, _value)) => {
                todo!()
            }

            (AvroSchema::Record(schema), Value::Record(items)) => builder
                .field_builder::<StructBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .and_then(|builder| append_struct_builder(schema, items, builder))?,

            (AvroSchema::Enum(_), Value::Enum(_, symbol)) => builder
                .field_builder::<StringDictionaryBuilder<UInt32Type>>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(symbol))?,

            (AvroSchema::Fixed(_fixed_schema), _) => todo!(),
            (AvroSchema::Decimal(_decimal_schema), _) => todo!(),
            (AvroSchema::BigDecimal, _) => todo!(),

            (AvroSchema::Uuid, Value::Uuid(value)) => builder
                .field_builder::<StringBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value.to_string()))?,

            (AvroSchema::Date, Value::Date(value)) => builder
                .field_builder::<Date32Builder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::TimeMillis, Value::TimeMillis(value)) => builder
                .field_builder::<Time32MillisecondBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::TimeMicros, Value::TimeMicros(value)) => builder
                .field_builder::<Time64MicrosecondBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::TimestampMillis, Value::TimestampMillis(value)) => builder
                .field_builder::<TimestampMillisecondBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::TimestampMicros, Value::TimeMicros(value)) => builder
                .field_builder::<TimestampMicrosecondBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::TimestampNanos, Value::TimestampNanos(value)) => builder
                .field_builder::<TimestampNanosecondBuilder>(index)
                .ok_or(Error::BadDowncast { field: name })
                .map(|values| values.append_value(value))?,

            (AvroSchema::LocalTimestampMillis, _) => todo!(),
            (AvroSchema::LocalTimestampMicros, _) => todo!(),
            (AvroSchema::LocalTimestampNanos, _) => todo!(),
            (AvroSchema::Duration, _) => todo!(),
            (AvroSchema::Ref { name }, _) => {
                let _ = name;
                todo!();
            }
            (schema, value) => unimplemented!("schema: {schema:?}, value: {value:?}"),
        }
    }

    builder.append(true);
    Ok(())
}

fn append_value(
    schema: Option<&AvroSchema>,
    value: Value,
    column: &mut Box<dyn ArrayBuilder>,
) -> Result<()> {
    debug!(?value);

    match (schema, value) {
        (Some(AvroSchema::Boolean), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<BooleanBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Int), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<Int32Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Long), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<Int64Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Float), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<Float32Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Double), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<Float64Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Bytes), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<BinaryBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::String), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Fixed(_)), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<BinaryBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Enum(_)), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<StringDictionaryBuilder<UInt32Type>>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Array(schema)), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<ListBuilder<Box<dyn ArrayBuilder>>>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema))
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Record(_)), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<StructBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Map(schema)), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<MapBuilder<Box<dyn ArrayBuilder>, Box<dyn ArrayBuilder>>>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema))
            .inspect(|_| debug!(?schema))
            .and_then(|builder| builder.append(true).map_err(Into::into)),

        (Some(AvroSchema::Date), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<Date32Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::TimeMillis), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<Time32MillisecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::TimeMicros), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<Time64MicrosecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::TimestampMillis), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<TimestampMillisecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::TimestampMicros), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<TimestampMicrosecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::LocalTimestampNanos), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<TimestampNanosecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (Some(AvroSchema::Uuid), Value::Null) => column
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_null()),

        (schema, Value::Null) => {
            debug!(?schema);
            todo!()
        }

        (_, Value::Boolean(value)) => column
            .as_any_mut()
            .downcast_mut::<BooleanBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::Int(value)) => column
            .as_any_mut()
            .downcast_mut::<Int32Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::Long(value)) => column
            .as_any_mut()
            .downcast_mut::<Int64Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::Float(value)) => column
            .as_any_mut()
            .downcast_mut::<Float32Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::Double(value)) => column
            .as_any_mut()
            .downcast_mut::<Float64Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::Bytes(value)) => column
            .as_any_mut()
            .downcast_mut::<BinaryBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::String(value)) => column
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::Fixed(_, value)) => column
            .as_any_mut()
            .downcast_mut::<BinaryBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (Some(AvroSchema::Enum(_)), Value::Enum(_, symbol)) => column
            .as_any_mut()
            .downcast_mut::<StringDictionaryBuilder<UInt32Type>>()
            .ok_or(Error::Downcast)
            .and_then(|builder| builder.append(symbol).and(Ok(())).map_err(Into::into)),

        (Some(AvroSchema::Union(schema)), Value::Union(_, value)) => {
            debug!(?schema, ?value);

            if let Some(schema) = schema.nullable_variant() {
                append_value(Some(schema), *value, column)
            } else {
                todo!()
            }
        }

        (Some(AvroSchema::Array(schema)), Value::Array(values)) => column
            .as_any_mut()
            .downcast_mut::<ListBuilder<Box<dyn ArrayBuilder>>>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .and_then(|builder| append_list_builder(schema, values, builder)),

        (Some(AvroSchema::Record(schema)), Value::Record(items)) => column
            .as_any_mut()
            .downcast_mut::<StructBuilder>()
            .ok_or(Error::Downcast)
            .and_then(|builder| append_struct_builder(schema, items, builder)),

        (Some(AvroSchema::Map(schema)), Value::Map(values)) => column
            .as_any_mut()
            .downcast_mut::<MapBuilder<Box<dyn ArrayBuilder>, Box<dyn ArrayBuilder>>>()
            .ok_or(Error::Downcast)
            .inspect_err(|err| error!(?err, ?schema, ?values))
            .inspect(|_| debug!(?schema, ?values))
            .and_then(|builder| append_map_builder(schema, values, builder)),

        (Some(AvroSchema::Date), Value::Date(value)) => column
            .as_any_mut()
            .downcast_mut::<Date32Builder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (schema, Value::Decimal(value)) => {
            let big_int = BigInt::from(value);
            todo!("schema: {schema:?}, value: {big_int:?}")
        }

        (schema, Value::BigDecimal(value)) => todo!("schema: {schema:?}, value: {value:?}"),

        (_, Value::TimeMillis(value)) => column
            .as_any_mut()
            .downcast_mut::<Time32MillisecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::TimeMicros(value)) => column
            .as_any_mut()
            .downcast_mut::<Time64MicrosecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::TimestampMillis(value)) => column
            .as_any_mut()
            .downcast_mut::<TimestampMillisecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::TimestampMicros(value)) => column
            .as_any_mut()
            .downcast_mut::<TimestampMicrosecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (_, Value::TimestampNanos(value)) => column
            .as_any_mut()
            .downcast_mut::<TimestampNanosecondBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value)),

        (schema, Value::LocalTimestampMillis(value)) => {
            todo!("schema: {schema:?}, value: {value:?}")
        }
        (schema, Value::LocalTimestampMicros(value)) => {
            todo!("schema: {schema:?}, value: {value:?}")
        }
        (schema, Value::LocalTimestampNanos(value)) => {
            todo!("schema: {schema:?}, value: {value:?}")
        }

        (schema, Value::Duration(value)) => todo!("schema: {schema:?}, value: {value:?}"),

        (_, Value::Uuid(value)) => column
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .ok_or(Error::Downcast)
            .map(|builder| builder.append_value(value.to_string())),

        (schema, value) => unimplemented!("schema: {schema:?}, value: {value:?}"),
    }
}

fn process(
    schema: Option<&AvroSchema>,
    encoded: Option<Bytes>,
    builders: &mut Vec<Box<dyn ArrayBuilder>>,
) -> Result<()> {
    schema.map_or(Ok(()), |schema| {
        builders
            .iter_mut()
            .next()
            .ok_or(Error::BuilderExhausted)
            .and_then(|builder| {
                encoded
                    .map_or(Err(Error::Api(ErrorCode::InvalidRecord)), |encoded| {
                        Reader::with_schema(schema, &encoded[..])?
                            .next()
                            .transpose()
                            .map_err(Into::into)
                    })
                    .inspect(|value| debug!(?value))
                    .and_then(|value| value.ok_or(Error::Api(ErrorCode::InvalidRecord)))
                    .and_then(|value| append_value(Some(schema), value, builder))
                    .inspect_err(|err| error!(?err, ?schema))
            })
    })
}

impl AsArrow for Schema {
    fn as_arrow(&self, batch: &Batch) -> Result<RecordBatch> {
        debug!(?batch);

        let schema = datafusion::arrow::datatypes::Schema::try_from(self)?;
        debug!(?schema);

        let mut record_builder = RecordBuilder::try_from(self)?;

        debug!(
            keys = record_builder.keys.len(),
            values = record_builder.values.len()
        );

        for record in &batch.records {
            debug!(?record);

            process(
                self.key.as_ref(),
                record.key.clone(),
                &mut record_builder.keys,
            )?;

            process(
                self.value.as_ref(),
                record.value.clone(),
                &mut record_builder.values,
            )?;
        }

        debug!(
            key_rows = ?record_builder.keys.iter().map(|rows| rows.len()).collect::<Vec<_>>(),
            value_rows = ?record_builder.values.iter().map(|rows| rows.len()).collect::<Vec<_>>()
        );

        let mut columns = vec![];
        columns.append(&mut record_builder.keys);
        columns.append(&mut record_builder.values);

        debug!(columns = columns.len());

        RecordBatch::try_new(
            schema.into(),
            columns.iter_mut().map(|builder| builder.finish()).collect(),
        )
        .map_err(Into::into)
    }
}

impl TryFrom<&Schema> for Fields {
    type Error = Error;

    fn try_from(schema: &Schema) -> Result<Self, Self::Error> {
        let mut fields = vec![];

        if let Some(ref schema) = schema.key {
            schema_data_type(schema)
                .map(|data_type| {
                    Field::new(
                        schema.name().map_or("key", |name| name.name.as_str()),
                        data_type,
                        NULLABLE,
                    )
                })
                .map(|field| fields.push(field))?;
        }

        if let Some(ref schema) = schema.value {
            schema_data_type(schema)
                .map(|data_type| {
                    Field::new(
                        schema.name().map_or("value", |name| name.name.as_str()),
                        data_type,
                        NULLABLE,
                    )
                })
                .map(|field| fields.push(field))?;
        }

        Ok(fields.into())
    }
}

impl TryFrom<&Schema> for datafusion::arrow::datatypes::Schema {
    type Error = Error;

    fn try_from(schema: &Schema) -> Result<Self, Self::Error> {
        Fields::try_from(schema).map(datafusion::arrow::datatypes::Schema::new)
    }
}

fn decode(validator: Option<&AvroSchema>, encoded: Option<Bytes>) -> Result<Option<Value>> {
    debug!(?validator, ?encoded);
    validator.map_or(Ok(None), |schema| {
        encoded.map_or(Err(Error::Api(ErrorCode::InvalidRecord)), |encoded| {
            apache_avro::Reader::with_schema(schema, &encoded[..])
                .and_then(|reader| reader.into_iter().next().transpose())
                .inspect(|value| debug!(?value))
                .inspect_err(|err| debug!(?err))
                .map_err(|_| Error::Api(ErrorCode::InvalidRecord))
                .and_then(|value| value.ok_or(Error::Api(ErrorCode::InvalidRecord)))
                .map(Some)
        })
    })
}

fn validate(validator: Option<&AvroSchema>, encoded: Option<Bytes>) -> Result<()> {
    decode(validator, encoded).and(Ok(()))
}

impl Validator for Schema {
    fn validate(&self, batch: &Batch) -> Result<()> {
        debug!(?batch);

        for record in &batch.records {
            debug!(?record);

            validate(self.key.as_ref(), record.key.clone())
                .and(validate(self.value.as_ref(), record.value.clone()))
                .inspect_err(|err| info!(?err, ?batch))?
        }

        Ok(())
    }
}

fn schema_write(schema: &AvroSchema, value: Value) -> Result<Bytes> {
    debug!(?schema, ?value);
    let mut writer = apache_avro::Writer::new(schema, vec![]);
    writer.append(value)?;
    writer.into_inner().map(Bytes::from).map_err(Into::into)
}

fn from_json(schema: &AvroSchema, json: &JsonValue) -> Result<Value> {
    debug!(?schema, ?json);

    match (schema, json) {
        (AvroSchema::Null, JsonValue::Null) => Ok(Value::Null),

        (AvroSchema::Boolean, JsonValue::Bool(value)) => Ok(Value::Boolean(*value)),

        (AvroSchema::Int, JsonValue::Number(value)) => value
            .as_i64()
            .ok_or(Error::JsonToAvro(schema.to_owned(), json.to_owned()))
            .and_then(|value| i32::try_from(value).map_err(Into::into))
            .map(Value::Int)
            .inspect_err(|err| debug!(?schema, ?json, ?err)),

        (AvroSchema::Long, JsonValue::Number(value)) => value
            .as_i64()
            .ok_or(Error::JsonToAvro(schema.to_owned(), json.to_owned()))
            .map(Value::Long),

        (AvroSchema::Double, JsonValue::Number(value)) => value
            .as_f64()
            .ok_or(Error::JsonToAvro(schema.to_owned(), json.to_owned()))
            .map(Value::Double)
            .inspect_err(|err| debug!(?schema, ?json, ?err)),

        (AvroSchema::Float, JsonValue::Number(value)) => value
            .as_f64()
            .ok_or(Error::JsonToAvro(schema.to_owned(), json.to_owned()))
            .map(|double| double as f32)
            .map(Value::Float)
            .inspect_err(|err| debug!(?schema, ?json, ?err)),

        (AvroSchema::Uuid, JsonValue::String(value)) => {
            Uuid::parse_str(value).map_err(Into::into).map(Value::Uuid)
        }

        (AvroSchema::Bytes, JsonValue::String(value)) => {
            Ok(Value::Bytes(value.as_bytes().to_vec()))
        }

        (AvroSchema::TimestampMillis, JsonValue::String(value)) => {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|date_time| date_time.and_utc().timestamp_millis())
                .map(Value::TimestampMillis)
                .inspect_err(|err| debug!(?err, value))
                .map_err(Into::into)
        }

        (AvroSchema::TimestampMicros, JsonValue::String(value)) => {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|date_time| date_time.and_utc().timestamp_micros())
                .map(Value::TimestampMicros)
                .inspect_err(|err| debug!(?err, value))
                .map_err(Into::into)
        }

        (AvroSchema::TimestampNanos, JsonValue::String(value)) => {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .inspect_err(|err| debug!(?err, value))
                .map_err(Into::into)
                .and_then(|date_time| {
                    date_time
                        .and_utc()
                        .timestamp_nanos_opt()
                        .ok_or(Error::JsonToAvro(schema.to_owned(), json.to_owned()))
                })
                .map(Value::TimestampNanos)
        }

        (AvroSchema::Enum(inner), JsonValue::String(value)) => inner
            .symbols
            .iter()
            .enumerate()
            .find(|(_, symbol)| *symbol == value)
            .ok_or(Error::JsonToAvro(schema.to_owned(), json.to_owned()))
            .and_then(|(index, symbol)| {
                u32::try_from(index)
                    .map(|index| (index, symbol))
                    .map_err(Into::into)
            })
            .map(|(index, symbol)| Value::Enum(index, symbol.to_owned())),

        (AvroSchema::String, JsonValue::String(value)) => Ok(Value::String(value.to_owned())),

        (AvroSchema::Array(schema), JsonValue::Array(values)) => values
            .iter()
            .map(|value| from_json(schema.items.as_ref(), value))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array)
            .inspect_err(|err| debug!(?schema, ?json, ?err)),

        (AvroSchema::Map(inner), JsonValue::Object(values)) => values
            .iter()
            .map(|(k, v)| from_json(inner.types.as_ref(), v).map(|v| (k.to_owned(), v)))
            .collect::<Result<HashMap<_, _>>>()
            .map(Value::Map),

        (AvroSchema::Record(record), JsonValue::Object(value)) => record
            .fields
            .iter()
            .map(|field| {
                value
                    .get(&field.name)
                    .ok_or(Error::JsonToAvroFieldNotFound {
                        schema: schema.to_owned(),
                        value: json.to_owned(),
                        field: field.name.clone(),
                    })
                    .and_then(|value| from_json(&field.schema, value))
                    .inspect(|value| debug!(name = ?field.name, ?value))
                    .map(|value| (field.name.clone(), value))
            })
            .collect::<Result<Vec<_>>>()
            .map(Value::Record)
            .inspect_err(|err| debug!(%err)),

        (schema, value) => Err(Error::JsonToAvro(schema.to_owned(), value.to_owned())),
    }
}

impl AsKafkaRecord for Schema {
    fn as_kafka_record(&self, value: &JsonValue) -> Result<tansu_kafka_sans_io::record::Builder> {
        let mut builder = tansu_kafka_sans_io::record::Record::builder();

        if let Some(value) = value.get("key") {
            debug!(?value);

            if let Some(ref schema) = self.key {
                builder = builder.key(
                    from_json(schema, value)
                        .and_then(|value| schema_write(schema, value))
                        .map(Into::into)?,
                );
            }
        }

        if let Some(value) = value.get("value") {
            debug!(?value);

            if let Some(ref schema) = self.value {
                builder = builder.value(
                    from_json(schema, value)
                        .and_then(|value| schema_write(schema, value))
                        .map(Into::into)?,
                );
            }
        }

        Ok(builder)
    }
}

fn json_value(value: Value) -> Result<JsonValue> {
    match value {
        Value::Null => Ok(JsonValue::Null),

        Value::Boolean(inner) => Ok(JsonValue::Bool(inner)),

        Value::Int(inner) => Ok(JsonValue::Number(Number::from(inner))),

        Value::Long(inner) => Ok(JsonValue::Number(Number::from(inner))),

        Value::Float(inner) => Number::from_f64(inner as f64)
            .ok_or(Error::AvroToJson(value.to_owned()))
            .map(JsonValue::Number),

        Value::Double(inner) => Number::from_f64(inner)
            .ok_or(Error::AvroToJson(value.to_owned()))
            .map(JsonValue::Number),

        Value::Bytes(inner) => Ok(JsonValue::String(String::from(String::from_utf8_lossy(
            &inner[..],
        )))),

        Value::String(inner) | Value::Enum(_, inner) => Ok(JsonValue::String(inner)),

        Value::Fixed(_, _) => todo!(),

        Value::Union(_, value) => json_value(*value),

        Value::Array(values) => values
            .into_iter()
            .map(json_value)
            .collect::<Result<Vec<_>>>()
            .map(JsonValue::Array),

        Value::Map(inner) => inner
            .into_iter()
            .map(|(k, v)| json_value(v).map(|v| (k, v)))
            .collect::<Result<Vec<_>>>()
            .map(Map::from_iter)
            .map(JsonValue::Object),

        Value::Record(inner) => inner
            .into_iter()
            .map(|(k, v)| json_value(v).map(|v| (k, v)))
            .collect::<Result<Vec<_>>>()
            .map(Map::from_iter)
            .map(JsonValue::Object),

        Value::Date(_) => todo!(),

        Value::Decimal(_decimal) => todo!(),
        Value::BigDecimal(_big_decimal) => todo!(),

        Value::TimeMillis(_) => todo!(),
        Value::TimeMicros(_) => todo!(),

        Value::TimestampMillis(_) => todo!(),
        Value::TimestampMicros(_) => todo!(),
        Value::TimestampNanos(_) => todo!(),

        Value::LocalTimestampMillis(_) => todo!(),
        Value::LocalTimestampMicros(_) => todo!(),
        Value::LocalTimestampNanos(_) => todo!(),

        Value::Duration(_duration) => todo!(),

        Value::Uuid(uuid) => json_value(Value::String(uuid.to_string())),
    }
}

impl Schema {
    fn to_json_value(
        &self,
        name: &str,
        schema: Option<&AvroSchema>,
        encoded: Option<Bytes>,
    ) -> Result<(String, JsonValue)> {
        decode(schema, encoded).and_then(|decoded| {
            decoded.map_or(Ok((name.to_owned(), JsonValue::Null)), |value| {
                json_value(value).map(|value| (name.to_owned(), value))
            })
        })
    }
}

impl AsJsonValue for Schema {
    fn as_json_value(&self, batch: &Batch) -> Result<JsonValue> {
        Ok(JsonValue::Array(
            batch
                .records
                .iter()
                .map(|record| {
                    JsonValue::Object(Map::from_iter(
                        self.to_json_value("key", self.key.as_ref(), record.key.clone())
                            .into_iter()
                            .chain(self.to_json_value(
                                "value",
                                self.value.as_ref(),
                                record.value.clone(),
                            )),
                    ))
                })
                .collect::<Vec<_>>(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::File, sync::Arc, thread};

    use crate::Registry;

    use super::*;
    use apache_avro::{Decimal, types::Value};
    use datafusion::{arrow::util::pretty::pretty_format_batches, prelude::*};
    use num_bigint::BigInt;
    use object_store::{ObjectStore, PutPayload, memory::InMemory, path::Path};
    use serde_json::json;
    use tansu_kafka_sans_io::record::Record;
    use tracing::subscriber::DefaultGuard;
    use tracing_subscriber::EnvFilter;
    use uuid::Uuid;

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

        let schema = json!({
            "type": "record",
            "name": "Test",
            "fields": [{
                "name": "key",
                "type": "int"
            },
            {
                "name": "value",
                "type": {
                    "type": "record",
                    "name": "person",
                    "fields": [{
                        "name": "name",
                        "type": "string"
                    },
                    {
                        "name": "email",
                        "type": "string"
                    }]
                }
        }]});

        let object_store = InMemory::new();
        {
            let location = Path::from(format!("{topic}.avsc"));
            _ = object_store
                .put(
                    &location,
                    serde_json::to_vec(&schema)
                        .map(Bytes::from)
                        .map(PutPayload::from)?,
                )
                .await?;
        }

        let registry = Registry::new(object_store);

        let key = AvroSchema::parse(&json!({
            "type": "int"
        }))
        .and_then(|schema| {
            let mut writer = apache_avro::Writer::new(&schema, vec![]);
            writer
                .append(Value::Int(32123))
                .and(writer.into_inner())
                .map(Bytes::from)
        })?;

        let batch = Batch::builder()
            .record(Record::builder().key(key.into()))
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

        let schema = json!({
            "type": "record",
            "name": "Test",
            "fields": [
                {"name": "key", "type": "int"},
                {"name": "value", "type": {
                    "type": "record",
                    "fields": [
                        {"name": "name", "type": "string"},
                        {"name": "email", "type": "string"}]}}]});

        let object_store = InMemory::new();
        {
            let location = Path::from(format!("{topic}/.avsc"));
            _ = object_store
                .put(
                    &location,
                    serde_json::to_vec(&schema)
                        .map(Bytes::from)
                        .map(PutPayload::from)?,
                )
                .await?;
        }

        let registry = Registry::new(object_store);

        let key = AvroSchema::parse(&json!({
            "type": "int"
        }))
        .and_then(|schema| {
            let mut writer = apache_avro::Writer::new(&schema, vec![]);
            writer
                .append(Value::Int(32123))
                .and(writer.into_inner())
                .map(Bytes::from)
        })?;

        let value = AvroSchema::parse(&json!({
            "type": "record",
            "name": "Message",
            "fields": [{"name": "name", "type": "string"}, {"name": "email", "type": "string"}]
        }))
        .and_then(|schema| {
            let mut writer = apache_avro::Writer::new(&schema, vec![]);
            let mut record = apache_avro::types::Record::new(&schema).unwrap();
            record.put("name", "alice");
            record.put("email", "alice@example.com");

            writer
                .append(record)
                .and(writer.into_inner())
                .map(Bytes::from)
        })?;

        let batch = Batch::builder()
            .record(Record::builder().key(key.into()).value(value.into()))
            .build()?;

        registry.validate(topic, &batch).await
    }

    #[tokio::test]
    async fn value_only_invalid_record() -> Result<()> {
        let _guard = init_tracing()?;

        let topic = "def";

        let schema = json!({
            "type": "record",
            "name": "Test",
            "fields": [
                {"name": "key", "type": "int"},
                {"name": "value", "type": {
                    "type": "record",
                    "fields": [
                        {"name": "name", "type": "string"},
                        {"name": "email", "type": "string"}]}}]});

        let object_store = InMemory::new();
        {
            let location = Path::from(format!("{topic}.avsc"));
            _ = object_store
                .put(
                    &location,
                    serde_json::to_vec(&schema)
                        .map(Bytes::from)
                        .map(PutPayload::from)?,
                )
                .await?;
        }

        {
            let location = Path::from(format!("{topic}/value.avsc"));
            _ = object_store.put(&location, serde_json::to_vec(&json!({
                        "type": "record",
                        "name": "Message",
                        "fields": [{"name": "name", "type": "string"}, {"name": "email", "type": "string"}]
                    }))
                    .map(Bytes::from)
                    .map(PutPayload::from)?).await?;
        }

        let registry = Registry::new(object_store);

        let value = AvroSchema::parse(&json!({
            "type": "record",
            "name": "Message",
            "fields": [{"name": "name", "type": "string"}, {"name": "email", "type": "string"}]
        }))
        .and_then(|schema| {
            let mut writer = apache_avro::Writer::new(&schema, vec![]);
            let mut record = apache_avro::types::Record::new(&schema).unwrap();
            record.put("name", "alice");
            record.put("email", "alice@example.com");

            writer
                .append(record)
                .and(writer.into_inner())
                .map(Bytes::from)
        })?;

        let batch = Batch::builder()
            .record(Record::builder().value(value.into()))
            .build()?;

        assert!(matches!(
            registry.validate(topic, &batch).await,
            Err(Error::Api(ErrorCode::InvalidRecord))
        ));

        Ok(())
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

    #[test]
    fn key() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{"name": "key", "type": "int"}]
        }));

        let input = {
            let mut writer = apache_avro::Writer::new(schema.key.as_ref().unwrap(), vec![]);

            writer
                .append(Value::Int(32123))
                .and(writer.into_inner())
                .map(Bytes::from)?
        };

        let batch = Batch::builder()
            .record(Record::builder().key(input.clone().into()))
            .build()?;

        schema.validate(&batch)
    }

    #[test]
    fn invalid_key() -> Result<()> {
        let _guard = init_tracing()?;

        let input = {
            let schema = Schema::from(&json!({
                "type": "record",
                "name": "test",
                "fields": [{"name": "key", "type": "long"}]
            }));

            let mut writer = apache_avro::Writer::new(schema.key.as_ref().unwrap(), vec![]);
            writer
                .append(Value::Long(32123))
                .and(writer.into_inner())
                .map(Bytes::from)?
        };

        let batch = Batch::builder()
            .record(Record::builder().key(input.clone().into()))
            .build()?;

        let s = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "key",
                "type": "string"
            }]
        }));

        assert!(matches!(
            s.validate(&batch),
            Err(Error::Api(ErrorCode::InvalidRecord))
        ));
        Ok(())
    }

    #[test]
    fn simple_schema() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = json!({
            "type": "record",
            "name": "Message",
            "fields": [{"name": "title", "type": "string"}, {"name": "message", "type": "string"}]
        });

        let schema = AvroSchema::parse(&schema)?;

        let mut record = apache_avro::types::Record::new(&schema).unwrap();
        record.put("title", "Lorem ipsum dolor sit amet");
        record.put("message", "consectetur adipiscing elit");

        let mut writer = apache_avro::Writer::new(&schema, vec![]);
        assert!(writer.append(record)? > 0);

        let input = writer.into_inner()?;
        let reader = apache_avro::Reader::with_schema(&schema, &input[..])?;

        let v = reader.into_iter().next().unwrap()?;

        assert_eq!(
            Value::Record(vec![
                (
                    "title".into(),
                    Value::String("Lorem ipsum dolor sit amet".into()),
                ),
                (
                    "message".into(),
                    Value::String("consectetur adipiscing elit".into()),
                ),
            ]),
            v
        );

        Ok(())
    }

    #[tokio::test]
    async fn record_of_primitive_data_types() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "Message",
            "fields": [
                {"name": "value", "type": "record", "fields": [
                {"name": "a", "type": "null"},
                {"name": "b", "type": "boolean"},
                {"name": "c", "type": "int"},
                {"name": "d", "type": "long"},
                {"name": "e", "type": "float"},
                {"name": "f", "type": "double"},
                {"name": "g", "type": "bytes"},
                {"name": "h", "type": "string"}
                ]}
            ]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [r(
                schema.value.as_ref().unwrap(),
                [
                    ("a", Value::Null),
                    ("b", false.into()),
                    ("c", i32::MAX.into()),
                    ("d", i64::MAX.into()),
                    ("e", f32::MAX.into()),
                    ("f", f64::MAX.into()),
                    ("g", Vec::from(&b"abcdef"[..]).into()),
                    ("h", "pqr".into()),
                ],
            )];

            for value in values {
                batch = batch.record(
                    Record::builder()
                        .value(schema_write(schema.value.as_ref().unwrap(), value.into())?.into()),
                )
            }
            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+-----------------------------------------------------------------------------------------------------------------------------+",
            "| value                                                                                                                       |",
            "+-----------------------------------------------------------------------------------------------------------------------------+",
            "| {a: , b: false, c: 2147483647, d: 9223372036854775807, e: 3.4028235e38, f: 1.7976931348623157e308, g: 616263646566, h: pqr} |",
            "+-----------------------------------------------------------------------------------------------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn record_of_with_list_of_primitive_data_types() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "Message",
            "fields": [
                {"name": "value", "type": "record", "fields": [
                    {"name": "b", "type": "array", "items": "boolean"},
                    {"name": "c", "type": "array", "items": "int"},
                    {"name": "d", "type": "array", "items": "long"},
                    {"name": "e", "type": "array", "items": "float"},
                    {"name": "f", "type": "array", "items": "double"},
                    {"name": "g", "type": "array", "items": "bytes"},
                    {"name": "h", "type": "array", "items": "string"}
                ]}
            ]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [r(
                schema.value.as_ref().unwrap(),
                [
                    ("b", Value::Array(vec![false.into(), true.into()])),
                    (
                        "c",
                        Value::Array(vec![i32::MIN.into(), 0.into(), i32::MAX.into()]),
                    ),
                    (
                        "d",
                        Value::Array(vec![i64::MIN.into(), 0.into(), i64::MAX.into()]),
                    ),
                    (
                        "e",
                        Value::Array(vec![f32::MIN.into(), 0.0f32.into(), f32::MAX.into()]),
                    ),
                    (
                        "f",
                        Value::Array(vec![f64::MIN.into(), 0.0f64.into(), f64::MAX.into()]),
                    ),
                    ("g", Value::Array(vec![Vec::from(&b"abcdef"[..]).into()])),
                    (
                        "h",
                        Value::Array(vec!["abc".into(), "pqr".into(), "xyz".into()]),
                    ),
                ],
            )];

            for value in values {
                batch = batch.record(
                    Record::builder()
                        .value(schema_write(schema.value.as_ref().unwrap(), value.into())?.into()),
                )
            }

            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------+",
            "| value                                                                                                                                                                                                                                           |",
            "+-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------+",
            "| {b: [false, true], c: [-2147483648, 0, 2147483647], d: [-9223372036854775808, 0, 9223372036854775807], e: [-3.4028235e38, 0.0, 3.4028235e38], f: [-1.7976931348623157e308, 0.0, 1.7976931348623157e308], g: [616263646566], h: [abc, pqr, xyz]} |",
            "+-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[ignore]
    #[test]
    fn union_with_ref() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = json!({
            "type": "record",
            "name": "LongList",
            "fields": [{"name": "next", "type": ["null", "LongList"]}]
        });

        let data_type = AvroSchema::parse(&schema)
            .map_err(Into::into)
            .and_then(|schema| schema_data_type(&schema))?;

        assert!(matches!(data_type, DataType::Struct(_)));

        let record = match data_type {
            DataType::Struct(record) => Some(record),
            _ => None,
        }
        .unwrap();

        let next = record[0].clone();
        assert_eq!("next", next.name());
        assert!(matches!(next.data_type(), DataType::Union(_, _)));

        Ok(())
    }
    #[tokio::test]
    async fn union() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "union",
            "fields": [{"name": "value", "type": ["null", "float"]}]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                Value::Union(1, Box::new(Value::Float(f32::MIN))),
                Value::Union(0, Box::new(Value::Null)),
                Value::Union(1, Box::new(Value::Float(f32::MAX))),
            ];

            for value in values {
                batch = batch.record(
                    Record::builder()
                        .value(schema_write(schema.value.as_ref().unwrap(), value)?.into()),
                )
            }
            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------+",
            "| value         |",
            "+---------------+",
            "| -3.4028235e38 |",
            "|               |",
            "| 3.4028235e38  |",
            "+---------------+",
        ]
        .into_iter()
        .collect::<Vec<_>>();

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn enumeration() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "Suit",
            "fields": [
                {
                    "name": "value",
                    "type": "enum",
                    "symbols": ["SPADES", "HEARTS", "DIAMONDS", "CLUBS"]
                }
            ]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [Value::from(json!("CLUBS")), Value::from(json!("HEARTS"))];

            for value in values {
                batch = batch.record(
                    Record::builder()
                        .value(schema_write(schema.value.as_ref().unwrap(), value)?.into()),
                )
            }
            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+--------+",
            "| value  |",
            "+--------+",
            "| CLUBS  |",
            "| HEARTS |",
            "+--------+",
        ]
        .into_iter()
        .collect::<Vec<_>>();

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn observation_enumeration() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "observation",
            "fields": [
                { "name": "key", "type": "string", "logicalType": "uuid" },
                {
                    "name": "value",
                    "type": "record",
                    "fields": [
                        { "name": "amount", "type": "double" },
                        { "name": "unit", "type": "enum", "symbols": ["CELSIUS", "MILLIBAR"] }
                    ]
                }
            ]
        }
        ));

        let batch = {
            let mut batch = Batch::builder();

            let values = [json!({
                "key": "1E44D9C2-5E7A-443B-BF10-2B1E5FD72F15",
                "value": {
                    "amount": 23.2,
                    "unit": "CELSIUS"
                }
            })];

            for value in values {
                batch = batch.record(schema.as_kafka_record(&value)?);
            }
            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+--------------------------------------+-------------------------------+",
            "| key                                  | value                         |",
            "+--------------------------------------+-------------------------------+",
            "| 1e44d9c2-5e7a-443b-bf10-2b1e5fd72f15 | {amount: 23.2, unit: CELSIUS} |",
            "+--------------------------------------+-------------------------------+",
        ]
        .into_iter()
        .collect::<Vec<_>>();

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[test]
    fn array() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = json!({
            "type": "array",
            "items": "string",
            "default": []
        });

        let data_type = AvroSchema::parse(&schema)
            .map_err(Into::into)
            .and_then(|schema| schema_data_type(&schema))?;

        assert!(matches!(data_type, DataType::List(_)));

        let field = match data_type {
            DataType::List(field) => Some(field),
            _ => None,
        }
        .unwrap();

        assert_eq!("item", field.name());
        assert_eq!(&DataType::Utf8, field.data_type());

        Ok(())
    }

    #[tokio::test]
    async fn map() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "Long",
            "fields": [
                {"name": "value", "type": "map", "values": "long", "default": {}},
            ],
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [Value::from(json!({"a": 1, "b": 3, "c": 5}))];

            for value in values {
                batch = batch.record(
                    Record::builder()
                        .value(schema_write(schema.value.as_ref().unwrap(), value)?.into()),
                )
            }
            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        fn sort(s: &str) -> String {
            let mut chars = s.chars().collect::<Vec<_>>();
            chars.sort();
            chars.into_iter().collect()
        }

        let expected = vec![
            "+--------------------+",
            "| value              |",
            "+--------------------+",
            "| {c: 5, a: 1, b: 3} |",
            "+--------------------+",
        ]
        .into_iter()
        .map(sort)
        .collect::<Vec<_>>();

        assert_eq!(
            pretty_results.trim().lines().map(sort).collect::<Vec<_>>(),
            expected
        );

        Ok(())
    }

    #[test]
    fn fixed() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = json!({
            "type": "fixed",
            "size": 16,
            "name": "md5"
        });

        let data_type = AvroSchema::parse(&schema)
            .map_err(Into::into)
            .and_then(|schema| schema_data_type(&schema))?;

        assert!(matches!(data_type, DataType::FixedSizeBinary(16)));

        Ok(())
    }

    #[tokio::test]
    async fn simple_integer_key_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [
                {"name": "key", "type": "int"}
            ]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let keys = [32123, 45654, 87678, 12321];

            for key in keys {
                batch = batch.record(
                    Record::builder()
                        .key(schema_write(schema.key.as_ref().unwrap(), key.into())?.into()),
                );
            }

            batch.build()
        }?;

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+-------+",
            "| key   |",
            "+-------+",
            "| 32123 |",
            "| 45654 |",
            "| 87678 |",
            "| 12321 |",
            "+-------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    fn r<'a>(
        schema: &AvroSchema,
        fields: impl IntoIterator<Item = (&'a str, Value)>,
    ) -> apache_avro::types::Record {
        apache_avro::types::Record::new(schema)
            .map(|mut record| {
                for (name, value) in fields {
                    record.put(name, value);
                }
                record
            })
            .unwrap()
    }

    #[tokio::test]
    async fn simple_record_value_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "Person",
            "fields": [{
                "name": "value",
                "type": "record",
                "fields": [
                    {"name": "id", "type": "int"},
                    {"name": "name", "type": "string"},
                    {"name": "lucky", "type": "array", "items": "int", "default": []}
                ]}
            ]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                r(
                    schema.value.as_ref().unwrap(),
                    [
                        ("id", 32123.into()),
                        ("name", "alice".into()),
                        ("lucky", Value::Array([6.into()].into())),
                    ],
                ),
                r(
                    schema.value.as_ref().unwrap(),
                    [
                        ("id", 45654.into()),
                        ("name", "bob".into()),
                        ("lucky", Value::Array([5.into(), 9.into()].into())),
                    ],
                ),
            ];

            for value in values {
                batch = batch.record(
                    Record::builder()
                        .value(schema_write(schema.value.as_ref().unwrap(), value.into())?.into()),
                )
            }
            batch.build()?
        };

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------------------------------+",
            "| value                                 |",
            "+---------------------------------------+",
            "| {id: 32123, name: alice, lucky: [6]}  |",
            "| {id: 45654, name: bob, lucky: [5, 9]} |",
            "+---------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_bool_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "array",
                "items": "boolean",
                "default": []
            }]
        }));

        let values = [[true, true], [false, true], [true, false], [false, false]]
            .into_iter()
            .map(|l| Value::Array(l.into_iter().map(Value::Boolean).collect::<Vec<_>>()))
            .collect::<Vec<_>>();

        let batch = {
            let mut batch = Batch::builder();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+----------------+",
            "| value          |",
            "+----------------+",
            "| [true, true]   |",
            "| [false, true]  |",
            "| [true, false]  |",
            "| [false, false] |",
            "+----------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_int_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "array",
                "items": "int",
                "default": []
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [vec![32123, 23432, 12321, 56765], vec![i32::MIN, i32::MAX]]
                .into_iter()
                .map(|l| Value::Array(l.into_iter().map(Value::Int).collect::<Vec<_>>()))
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+------------------------------+",
            "| value                        |",
            "+------------------------------+",
            "| [32123, 23432, 12321, 56765] |",
            "| [-2147483648, 2147483647]    |",
            "+------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_long_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "array",
                "items": "long",
                "default": []
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [vec![32123, 23432, 12321, 56765], vec![i64::MIN, i64::MAX]]
                .into_iter()
                .map(|l| Value::Array(l.into_iter().map(Value::Long).collect::<Vec<_>>()))
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------------------------------------+",
            "| value                                       |",
            "+---------------------------------------------+",
            "| [32123, 23432, 12321, 56765]                |",
            "| [-9223372036854775808, 9223372036854775807] |",
            "+---------------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_float_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "name": "test",
            "type": "record",
            "fields": [{
                "name": "value",
                "type": "array",
                "items": "float",
                "default": []
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                vec![3.2123, 23.432, 123.21, 5676.5],
                vec![f32::MIN, f32::MAX],
            ]
            .into_iter()
            .map(|l| Value::Array(l.into_iter().map(Value::Float).collect::<Vec<_>>()))
            .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+----------------------------------+",
            "| value                            |",
            "+----------------------------------+",
            "| [3.2123, 23.432, 123.21, 5676.5] |",
            "| [-3.4028235e38, 3.4028235e38]    |",
            "+----------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_double_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
               "name": "value",
                "type": "array",
                "items": "double",
                "default": []
            }],
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                vec![3.2123, 23.432, 123.21, 5676.5],
                vec![f64::MIN, f64::MAX],
            ]
            .into_iter()
            .map(|l| Value::Array(l.into_iter().map(Value::Double).collect::<Vec<_>>()))
            .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------------------------------------------+",
            "| value                                             |",
            "+---------------------------------------------------+",
            "| [3.2123, 23.432, 123.21, 5676.5]                  |",
            "| [-1.7976931348623157e308, 1.7976931348623157e308] |",
            "+---------------------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_string_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "array",
                "items": "string",
                "default": []
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                vec!["abc".to_string(), "def".to_string(), "pqr".to_string()],
                vec!["xyz".to_string()],
            ]
            .into_iter()
            .map(|l| Value::Array(l.into_iter().map(Value::String).collect::<Vec<_>>()))
            .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+-----------------+",
            "| value           |",
            "+-----------------+",
            "| [abc, def, pqr] |",
            "| [xyz]           |",
            "+-----------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_record_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "array",
                "items": {
                    "type": "record",
                    "name": "xyz",
                    "fields": [{
                        "name": "id",
                        "type": "int"
                    },
                    {
                        "name": "name",
                        "type": "string"
                    }
                ]},
                "default": []
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                Value::Array(vec![
                    Value::Record(vec![
                        ("id".into(), 32123.into()),
                        ("name".into(), "alice".into()),
                    ]),
                    Value::Record(vec![
                        ("id".into(), 45654.into()),
                        ("name".into(), "bob".into()),
                    ]),
                ]),
                Value::Array(vec![Value::Record(vec![
                    ("id".into(), 54345.into()),
                    ("name".into(), "betty".into()),
                ])]),
            ];

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+----------------------------------------------------+",
            "| value                                              |",
            "+----------------------------------------------------+",
            "| [{id: 32123, name: alice}, {id: 45654, name: bob}] |",
            "| [{id: 54345, name: betty}]                         |",
            "+----------------------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn array_bytes_value() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "array",
                "items": "bytes",
                "default": []
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                vec![b"abc".to_vec(), b"def".to_vec(), b"pqr".to_vec()],
                vec![b"54345".to_vec()],
            ]
            .into_iter()
            .map(|l| Value::Array(l.into_iter().map(Value::Bytes).collect::<Vec<_>>()))
            .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+--------------------------+",
            "| value                    |",
            "+--------------------------+",
            "| [616263, 646566, 707172] |",
            "| [3534333435]             |",
            "+--------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn uuid_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "string",
                "logicalType": "uuid"
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                "383BB977-7D38-42B5-8BE7-58A1C606DE7A",
                "2C1FDDC8-4EBE-43FD-8F1C-47E18B7A4E21",
                "F9B45334-9AA2-4978-8735-9800D27A551C",
            ]
            .into_iter()
            .map(|uuid| Uuid::parse_str(uuid).map(Value::Uuid).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?;

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+--------------------------------------+",
            "| value                                |",
            "+--------------------------------------+",
            "| 383bb977-7d38-42b5-8be7-58a1c606de7a |",
            "| 2c1fddc8-4ebe-43fd-8f1c-47e18b7a4e21 |",
            "| f9b45334-9aa2-4978-8735-9800d27a551c |",
            "+--------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn time_millis_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "int",
                "logicalType": "time-millis"
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [1, 2, 3]
                .into_iter()
                .map(Value::TimeMillis)
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+--------------+",
            "| value        |",
            "+--------------+",
            "| 00:00:00.001 |",
            "| 00:00:00.002 |",
            "| 00:00:00.003 |",
            "+--------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn time_micros_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "long",
                "logicalType": "time-micros"
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [1, 2, 3]
                .into_iter()
                .map(Value::TimeMicros)
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+-----------------+",
            "| value           |",
            "+-----------------+",
            "| 00:00:00.000001 |",
            "| 00:00:00.000002 |",
            "| 00:00:00.000003 |",
            "+-----------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn timestamp_millis_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "long",
                "logicalType": "timestamp-millis"
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [119_731_017, 1_000_000_000, 1_234_567_890]
                .into_iter()
                .map(|seconds| Value::TimestampMillis(seconds * 1_000))
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------------+",
            "| value               |",
            "+---------------------+",
            "| 1973-10-17T18:36:57 |",
            "| 2001-09-09T01:46:40 |",
            "| 2009-02-13T23:31:30 |",
            "+---------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn timestamp_micros_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "long",
                "logicalType": "timestamp-micros"
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [119_731_017, 1_000_000_000, 1_234_567_890]
                .into_iter()
                .map(|seconds| Value::TimestampMicros(seconds * 1_000 * 1_000))
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------------+",
            "| value               |",
            "+---------------------+",
            "| 1973-10-17T18:36:57 |",
            "| 2001-09-09T01:46:40 |",
            "| 2009-02-13T23:31:30 |",
            "+---------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[ignore]
    #[tokio::test]
    async fn local_timestamp_millis_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
                "fields": [{
                    "name": "value",
                    "type": "long",
                    "logicalType": "local-timestamp-millis"
                }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [119_731_017, 1_000_000_000, 1_234_567_890]
                .into_iter()
                .map(|seconds| Value::LocalTimestampMillis(seconds * 1_000))
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------------+",
            "| value               |",
            "+---------------------+",
            "| 1973-10-17T18:36:57 |",
            "| 2001-09-09T01:46:40 |",
            "| 2009-02-13T23:31:30 |",
            "+---------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[ignore]
    #[tokio::test]
    async fn local_timestamp_micros_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "long",
                "logicalType": "local-timestamp-micros"
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [119_731_017, 1_000_000_000, 1_234_567_890]
                .into_iter()
                .map(|seconds| Value::LocalTimestampMicros(seconds * 1_000 * 1_000))
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+---------------------+",
            "| value               |",
            "+---------------------+",
            "| 1973-10-17T18:36:57 |",
            "| 2001-09-09T01:46:40 |",
            "| 2009-02-13T23:31:30 |",
            "+---------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn date_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "int",
                "logicalType": "date"
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [
                Value::Int(1),
                Value::Int(1_385),
                Value::Int(11_574),
                Value::Int(14_288),
            ];

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+------------+",
            "| value      |",
            "+------------+",
            "| 1970-01-02 |",
            "| 1973-10-17 |",
            "| 2001-09-09 |",
            "| 2009-02-13 |",
            "+------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[ignore]
    #[tokio::test]
    async fn decimal_fixed_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": {
                    "type": "fixed",
                    "size": 8,
                    "name": "decimal"
                },
                "logicalType": "decimal",
                "precision": 8,
                "scale": 2,
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [32123, 45654, 87678, 12321]
                .into_iter()
                .map(BigInt::from)
                .map(|big_int| big_int.to_signed_bytes_be())
                .map(Decimal::from)
                .map(Value::Decimal)
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+------------+",
            "| value      |",
            "+------------+",
            "| 1970-01-02 |",
            "| 1973-10-17 |",
            "| 2001-09-09 |",
            "| 2009-02-13 |",
            "+------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[ignore]
    #[tokio::test]
    async fn decimal_variable_logical_type() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "value",
                "type": "bytes",
                "logicalType": "decimal",
                "precision": 8,
                "scale": 2,
            }]
        }));

        let batch = {
            let mut batch = Batch::builder();

            let values = [32123, 45654, 87678, 12321]
                .into_iter()
                .map(BigInt::from)
                .map(|big_int| big_int.to_signed_bytes_be())
                .map(Decimal::from)
                .map(Value::Decimal)
                .collect::<Vec<_>>();

            for value in values {
                batch = batch.record(
                    Record::builder().value(
                        schema_write(schema.value.as_ref().unwrap(), value)
                            .inspect(|encoded| debug!(?encoded))?
                            .into(),
                    ),
                )
            }

            batch.build()
        }?;

        debug!(?batch);

        let record_batch = schema.as_arrow(&batch)?;
        debug!(?record_batch);

        let ctx = SessionContext::new();

        _ = ctx.register_batch("t", record_batch)?;
        let df = ctx.sql("select * from t").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+------------+",
            "| value      |",
            "+------------+",
            "| 1970-01-02 |",
            "| 1973-10-17 |",
            "| 2001-09-09 |",
            "| 2009-02-13 |",
            "+------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[tokio::test]
    async fn string_key_with_record_as_arrow() -> Result<()> {
        let _guard = init_tracing()?;

        let schema = Schema::from(&json!({
            "type": "record",
            "name": "test",
            "fields": [{
                "name": "key",
                "type": "string",
            },
            {
                "name": "value",
                "type": "record",
                "fields": [
                    {"name": "first", "type": "string"},
                    {"name": "last", "type": "string"},
                    {"name": "test1", "type": "double"},
                    {"name": "test2", "type": "double"},
                    {"name": "test3", "type": "double"},
                    {"name": "test4", "type": "double"},
                    {"name": "final", "type": "double"},
                    {"name": "grade", "type": "string"}
                ]
            }]

        }));

        // https://people.math.sc.edu/Burkardt/datasets/csv/csv.html
        let grades = [
            (
                "Alfalfa",
                "Aloysius",
                "123-45-6789",
                40.0,
                90.0,
                100.0,
                83.0,
                49.0,
                "D-",
            ),
            (
                "Alfred",
                "University",
                "123-12-1234",
                41.0,
                97.0,
                96.0,
                97.0,
                48.0,
                "D+",
            ),
            (
                "Gerty",
                "Gramma",
                "567-89-0123",
                41.0,
                80.0,
                60.0,
                40.0,
                44.0,
                "C",
            ),
            (
                "Android",
                "Electric",
                "087-65-4321",
                42.0,
                23.0,
                36.0,
                45.0,
                47.0,
                "B-",
            ),
            (
                "Bumpkin",
                "Fred",
                "456-78-9012",
                43.0,
                78.0,
                88.0,
                77.0,
                45.0,
                "A-",
            ),
            (
                "Rubble",
                "Betty",
                "234-56-7890",
                44.0,
                90.0,
                80.0,
                90.0,
                46.0,
                "C-",
            ),
            (
                "Noshow",
                "Cecil",
                "345-67-8901",
                45.0,
                11.0,
                -1.0,
                4.0,
                43.0,
                "F",
            ),
            (
                "Buff",
                "Bif",
                "632-79-9939",
                46.0,
                20.0,
                30.0,
                40.0,
                50.0,
                "B+",
            ),
            (
                "Airpump",
                "Andrew",
                "223-45-6789",
                49.0,
                1.0,
                90.0,
                100.0,
                83.0,
                "A",
            ),
            (
                "Backus",
                "Jim",
                "143-12-1234",
                48.0,
                1.0,
                97.0,
                96.0,
                97.0,
                "A+",
            ),
            (
                "Carnivore",
                "Art",
                "565-89-0123",
                44.0,
                1.0,
                80.0,
                60.0,
                40.0,
                "D+",
            ),
            (
                "Dandy",
                "Jim",
                "087-75-4321",
                47.0,
                1.0,
                23.0,
                36.0,
                45.0,
                "C+",
            ),
            (
                "Elephant",
                "Ima",
                "456-71-9012",
                45.0,
                1.0,
                78.0,
                88.0,
                77.0,
                "B-",
            ),
            (
                "Franklin",
                "Benny",
                "234-56-2890",
                50.0,
                1.0,
                90.0,
                80.0,
                90.0,
                "B-",
            ),
            (
                "George",
                "Boy",
                "345-67-3901",
                40.0,
                1.0,
                11.0,
                -1.0,
                4.0,
                "B",
            ),
            (
                "Heffalump",
                "Harvey",
                "632-79-9439",
                30.0,
                1.0,
                20.0,
                30.0,
                40.0,
                "C",
            ),
        ];

        let batch = {
            let mut batch = Batch::builder();

            for grade in grades {
                let mut value =
                    apache_avro::types::Record::new(schema.value.as_ref().unwrap()).unwrap();
                value.put("first", grade.0);
                value.put("last", grade.1);
                value.put("test1", grade.3);
                value.put("test2", grade.4);
                value.put("test3", grade.5);
                value.put("test4", grade.6);
                value.put("final", grade.7);
                value.put("grade", grade.8);

                batch = batch.record(
                    Record::builder()
                        .key(schema_write(schema.key.as_ref().unwrap(), grade.2.into())?.into())
                        .value(schema_write(schema.value.as_ref().unwrap(), value.into())?.into()),
                );
            }

            batch.build()
        }?;

        let record_batch = schema.as_arrow(&batch)?;

        let ctx = SessionContext::new();

        _ = ctx.register_batch("search", record_batch)?;
        let df = ctx.sql("select * from search").await?;
        let results = df.collect().await?;

        let pretty_results = pretty_format_batches(&results).map(|pretty| pretty.to_string())?;

        let expected = vec![
            "+-------------+---------------------------------------------------------------------------------------------------------------+",
            "| key         | value                                                                                                         |",
            "+-------------+---------------------------------------------------------------------------------------------------------------+",
            "| 123-45-6789 | {first: Alfalfa, last: Aloysius, test1: 40.0, test2: 90.0, test3: 100.0, test4: 83.0, final: 49.0, grade: D-} |",
            "| 123-12-1234 | {first: Alfred, last: University, test1: 41.0, test2: 97.0, test3: 96.0, test4: 97.0, final: 48.0, grade: D+} |",
            "| 567-89-0123 | {first: Gerty, last: Gramma, test1: 41.0, test2: 80.0, test3: 60.0, test4: 40.0, final: 44.0, grade: C}       |",
            "| 087-65-4321 | {first: Android, last: Electric, test1: 42.0, test2: 23.0, test3: 36.0, test4: 45.0, final: 47.0, grade: B-}  |",
            "| 456-78-9012 | {first: Bumpkin, last: Fred, test1: 43.0, test2: 78.0, test3: 88.0, test4: 77.0, final: 45.0, grade: A-}      |",
            "| 234-56-7890 | {first: Rubble, last: Betty, test1: 44.0, test2: 90.0, test3: 80.0, test4: 90.0, final: 46.0, grade: C-}      |",
            "| 345-67-8901 | {first: Noshow, last: Cecil, test1: 45.0, test2: 11.0, test3: -1.0, test4: 4.0, final: 43.0, grade: F}        |",
            "| 632-79-9939 | {first: Buff, last: Bif, test1: 46.0, test2: 20.0, test3: 30.0, test4: 40.0, final: 50.0, grade: B+}          |",
            "| 223-45-6789 | {first: Airpump, last: Andrew, test1: 49.0, test2: 1.0, test3: 90.0, test4: 100.0, final: 83.0, grade: A}     |",
            "| 143-12-1234 | {first: Backus, last: Jim, test1: 48.0, test2: 1.0, test3: 97.0, test4: 96.0, final: 97.0, grade: A+}         |",
            "| 565-89-0123 | {first: Carnivore, last: Art, test1: 44.0, test2: 1.0, test3: 80.0, test4: 60.0, final: 40.0, grade: D+}      |",
            "| 087-75-4321 | {first: Dandy, last: Jim, test1: 47.0, test2: 1.0, test3: 23.0, test4: 36.0, final: 45.0, grade: C+}          |",
            "| 456-71-9012 | {first: Elephant, last: Ima, test1: 45.0, test2: 1.0, test3: 78.0, test4: 88.0, final: 77.0, grade: B-}       |",
            "| 234-56-2890 | {first: Franklin, last: Benny, test1: 50.0, test2: 1.0, test3: 90.0, test4: 80.0, final: 90.0, grade: B-}     |",
            "| 345-67-3901 | {first: George, last: Boy, test1: 40.0, test2: 1.0, test3: 11.0, test4: -1.0, final: 4.0, grade: B}           |",
            "| 632-79-9439 | {first: Heffalump, last: Harvey, test1: 30.0, test2: 1.0, test3: 20.0, test4: 30.0, final: 40.0, grade: C}    |",
            "+-------------+---------------------------------------------------------------------------------------------------------------+",
        ];

        assert_eq!(pretty_results.trim().lines().collect::<Vec<_>>(), expected);

        Ok(())
    }

    #[test]
    fn from_json() -> Result<()> {
        let _guard = init_tracing()?;

        assert_eq!(
            Value::Null,
            super::from_json(&AvroSchema::parse(&json!({"type": "null"}))?, &json!(null))?
        );

        assert_eq!(
            Value::Boolean(true),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "boolean"}))?,
                &json!(true)
            )?
        );

        assert_eq!(
            Value::Boolean(false),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "boolean"}))?,
                &json!(false)
            )?
        );

        assert_eq!(
            Value::Int(i32::MIN),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "int"}))?,
                &json!(i32::MIN)
            )?
        );

        assert_eq!(
            Value::Int(i32::MAX),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "int"}))?,
                &json!(i32::MAX)
            )?
        );

        assert_eq!(
            Value::Long(i64::MIN),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "long"}))?,
                &json!(i64::MIN)
            )?
        );

        assert_eq!(
            Value::Long(i64::MAX),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "long"}))?,
                &json!(i64::MAX)
            )?
        );

        assert_eq!(
            Value::Float(f32::MIN),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "float"}))?,
                &json!(f32::MIN)
            )?
        );

        assert_eq!(
            Value::Float(f32::MAX),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "float"}))?,
                &json!(f32::MAX)
            )?
        );

        assert_eq!(
            Value::Double(f64::MIN),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "double"}))?,
                &json!(f64::MIN)
            )?
        );

        assert_eq!(
            Value::Double(f64::MAX),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "double"}))?,
                &json!(f64::MAX)
            )?
        );

        assert_eq!(
            Value::String("hello world!".into()),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "string"}))?,
                &json!("hello world!")
            )?
        );

        assert_eq!(
            Value::Array(vec![
                Value::String("abc".into()),
                Value::String("pqr".into()),
                Value::String("xyz".into()),
            ]),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "array", "items": "string"}))?,
                &json!(["abc", "pqr", "xyz"])
            )?
        );

        assert_eq!(
            Value::Enum(2, "DIAMONDS".into()),
            super::from_json(
                &AvroSchema::parse(&json!({
                    "type": "enum",
                    "name": "Suit",
                    "symbols": ["SPADES", "HEARTS", "DIAMONDS", "CLUBS"]
                }))?,
                &json!("DIAMONDS")
            )?
        );

        assert_eq!(
            Value::Bytes([97, 98, 99].into()),
            super::from_json(
                &AvroSchema::parse(&json!({"type": "bytes"}))?,
                &json!("abc")
            )?
        );

        {
            let uuid = Uuid::new_v4();

            assert_eq!(
                Value::Uuid(uuid),
                super::from_json(
                    &AvroSchema::parse(&json!({"type": "string", "logicalType": "uuid"}))?,
                    &json!(uuid.to_string())
                )?
            );
        }

        {
            let value = Value::TimestampMillis(119_731_017_000);

            assert_eq!(
                value,
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-millis"})
                    )?,
                    &json!("1973-10-17T18:36:57")
                )?
            );

            let value = Value::TimestampMillis(119_731_017_123);

            assert_eq!(
                value,
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-millis"})
                    )?,
                    &json!("1973-10-17T18:36:57.123")
                )?
            );

            assert_eq!(
                value,
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-millis"})
                    )?,
                    &json!("1973-10-17T18:36:57.123456")
                )?
            );

            assert_eq!(
                value,
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-millis"})
                    )?,
                    &json!("1973-10-17T18:36:57.123456789")
                )?
            );
        }

        {
            assert_eq!(
                Value::TimestampMicros(119_731_017_000_000),
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-micros"})
                    )?,
                    &json!("1973-10-17T18:36:57")
                )?
            );

            assert_eq!(
                Value::TimestampMicros(119_731_017_123_000),
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-micros"})
                    )?,
                    &json!("1973-10-17T18:36:57.123")
                )?
            );

            assert_eq!(
                Value::TimestampMicros(119_731_017_123_456),
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-micros"})
                    )?,
                    &json!("1973-10-17T18:36:57.123456")
                )?
            );

            assert_eq!(
                Value::TimestampMicros(119_731_017_123_456),
                super::from_json(
                    &AvroSchema::parse(
                        &json!({"type": "long", "logicalType": "timestamp-micros"})
                    )?,
                    &json!("1973-10-17T18:36:57.123456789")
                )?
            );
        }

        {
            let v = super::from_json(
                &AvroSchema::parse(&json!({
                    "type": "map",
                    "values": "long"
                }))?,
                &json!({"a": 1, "b": 3, "c": 5}),
            )?;

            assert!(matches!(v, Value::Map(_)));

            let Value::Map(values) = v else {
                panic!("{v:?}")
            };

            assert_eq!(Some(&Value::Long(1)), values.get("a"));
            assert_eq!(Some(&Value::Long(3)), values.get("b"));
            assert_eq!(Some(&Value::Long(5)), values.get("c"));
        }

        {
            let v = super::from_json(
                &AvroSchema::parse(&json!({
                "type": "array",
                "items": {
                    "type": "record",
                    "name": "people",
                    "fields": [
                        {"name": "id", "type": "int"},
                        {"name": "name", "type": "string"},
                        {"name": "lucky", "type": "array", "items": "int"}
                    ]}
                }))?,
                &json!([
                    {"id": 32123, "name": "alice", "lucky": [6]},
                    {"id": 45654, "name": "bob", "lucky": [5, 9]}]),
            )?;

            assert!(matches!(v, Value::Array(_)));

            let Value::Array(values) = v else {
                panic!("{v:?}")
            };

            assert_eq!(2, values.len());

            let Some(Value::Record(r0)) = values.first() else {
                panic!("{:?}", values[0])
            };

            assert_eq!(
                Value::Int(32123),
                r0.iter().find(|(name, _)| name == "id").unwrap().1
            );

            assert_eq!(
                Value::String("alice".into()),
                r0.iter().find(|(name, _)| name == "name").unwrap().1
            );

            assert_eq!(
                Value::Array(vec![Value::Int(6)]),
                r0.iter().find(|(name, _)| name == "lucky").unwrap().1
            );

            let Some(Value::Record(r1)) = values.get(1) else {
                panic!("{:?}", values[0])
            };

            assert_eq!(
                Value::Int(45654),
                r1.iter().find(|(name, _)| name == "id").unwrap().1
            );

            assert_eq!(
                Value::String("bob".into()),
                r1.iter().find(|(name, _)| name == "name").unwrap().1
            );

            assert_eq!(
                Value::Array(vec![Value::Int(5), Value::Int(9)]),
                r1.iter().find(|(name, _)| name == "lucky").unwrap().1
            );
        }

        Ok(())
    }
}

use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::Read;
use std::str::FromStr;
use std::sync::Arc;

use failure::Error;
use uuid::Uuid;

use crate::decimal::Decimal;
use crate::duration::Duration;
use crate::schema::Schema;
use crate::types::Value;
use crate::util::{safe_len, zag_i32, zag_i64, DecodeError};

#[inline]
fn decode_long<R: Read>(reader: &mut R) -> Result<Value, Error> {
    zag_i64(reader).map(Value::Long)
}

#[inline]
fn decode_int<R: Read>(reader: &mut R) -> Result<Value, Error> {
    zag_i32(reader).map(Value::Int)
}

#[inline]
fn decode_len<R: Read>(reader: &mut R) -> Result<usize, Error> {
    zag_i64(reader).and_then(|len| safe_len(len as usize))
}

/// Decode a `Value` from avro format given its `Schema`.
pub fn decode<R: Read>(schema: &Schema, reader: &mut R) -> Result<Value, Error> {
    match *schema {
        Schema::Null => Ok(Value::Null),
        Schema::Boolean => {
            let mut buf = [0u8; 1];
            reader.read_exact(&mut buf[..])?;

            match buf[0] {
                0u8 => Ok(Value::Boolean(false)),
                1u8 => Ok(Value::Boolean(true)),
                _ => Err(DecodeError::new("not a bool").into()),
            }
        }
        Schema::Decimal { ref inner, .. } => match **inner {
            Schema::Fixed { .. } => match decode(inner, reader)? {
                Value::Fixed(_, bytes) => Ok(Value::Decimal(Decimal::from(bytes))),
                _ => Err(DecodeError::new(
                    "not a fixed value, required for decimal with fixed schema",
                )
                .into()),
            },
            Schema::Bytes => match decode(inner, reader)? {
                Value::Bytes(bytes) => Ok(Value::Decimal(Decimal::from(bytes))),
                _ => Err(DecodeError::new(
                    "not a bytes value, required for decimal with bytes schema",
                )
                .into()),
            },
            _ => Err(
                DecodeError::new("not a fixed or bytes type, required for decimal schema").into(),
            ),
        },
        Schema::Uuid => Ok(Value::Uuid(Uuid::from_str(
            match decode(&Schema::String, reader)? {
                Value::String(ref s) => s,
                _ => return Err(DecodeError::new("not a string type, required for uuid").into()),
            },
        )?)),
        Schema::Int => decode_int(reader),
        Schema::Date => zag_i32(reader).map(Value::Date),
        Schema::TimeMillis => zag_i32(reader).map(Value::TimeMillis),
        Schema::Long => decode_long(reader),
        Schema::TimeMicros => zag_i64(reader).map(Value::TimeMicros),
        Schema::TimestampMillis => zag_i64(reader).map(Value::TimestampMillis),
        Schema::TimestampMicros => zag_i64(reader).map(Value::TimestampMicros),
        Schema::Duration => {
            let mut buf = [0u8; 12];
            reader.read_exact(&mut buf)?;
            Ok(Value::Duration(Duration::from(buf)))
        }
        Schema::Float => {
            let mut buf = [0u8; std::mem::size_of::<f32>()];
            reader.read_exact(&mut buf[..])?;
            Ok(Value::Float(f32::from_le_bytes(buf)))
        }
        Schema::Double => {
            let mut buf = [0u8; std::mem::size_of::<f64>()];
            reader.read_exact(&mut buf[..])?;
            Ok(Value::Double(f64::from_le_bytes(buf)))
        }
        Schema::Bytes => {
            let len = decode_len(reader)?;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;
            Ok(Value::Bytes(buf))
        }
        Schema::String => {
            let len = decode_len(reader)?;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;

            String::from_utf8(buf)
                .map(Value::String)
                .map_err(|_| DecodeError::new("not a valid utf-8 string").into())
        }
        Schema::Fixed { size, .. } => {
            let mut buf = vec![0u8; size as usize];
            reader.read_exact(&mut buf)?;
            Ok(Value::Fixed(size, buf))
        }
        Schema::Array(ref inner) => {
            let mut items = Vec::new();

            loop {
                let mut len = zag_i64(reader)?;
                // arrays are 0-terminated, 0i64 is also encoded as 0 in Avro
                // reading a length of 0 means the end of the array
                if len == 0 {
                    break;
                } else if len < 0 {
                    let _size = zag_i64(reader)?;
                    len = -len;
                }
                let len = safe_len(len as usize)?;

                items.reserve(len as usize);
                for _ in 0..len {
                    items.push(decode(inner, reader)?);
                }
            }

            Ok(Value::Array(items))
        }
        Schema::Map(ref inner) => {
            let mut items = HashMap::new();

            loop {
                let mut len = zag_i64(reader)?;
                // maps are 0-terminated, 0i64 is also encoded as 0 in Avro
                // reading a length of 0 means the end of the map
                if len == 0 {
                    break;
                } else if len < 0 {
                    let _size = zag_i64(reader)?;
                    len = -len;
                }
                let len = safe_len(len as usize)?;

                items.reserve(len as usize);
                for _ in 0..len {
                    if let Value::String(key) = decode(&Schema::String, reader)? {
                        let value = decode(inner, reader)?;
                        items.insert(key, value);
                    } else {
                        return Err(DecodeError::new("map key is not a string").into());
                    }
                }
            }

            Ok(Value::Map(items))
        }
        Schema::Union(ref inner) => {
            let index = zag_i64(reader)?;
            let variants = inner.variants();
            match variants.get(index as usize) {
                Some(variant) => decode(variant, reader).map(|x| Value::Union(Box::new(x))),
                None => Err(DecodeError::new("Union index out of bounds").into()),
            }
        }
        Schema::Record { ref fields, .. } => {
            // Benchmarks indicate ~10% improvement using this method.
            let mut items = Vec::with_capacity(fields.len());
            for field in fields.iter() {
                items.push((Arc::clone(&field.name), decode(&field.schema, reader)?));
            }
            Ok(Value::Record(items))
        }
        Schema::Enum { ref symbols, .. } => {
            if let Value::Int(index) = decode_int(reader)? {
                let symbol = Arc::clone(
                    symbols
                        .get(usize::try_from(index)?)
                        .ok_or_else(|| DecodeError::new("enum symbol index out of bounds"))?,
                );
                Ok(Value::Enum(index, symbol))
            } else {
                Err(DecodeError::new("enum symbol not found").into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value::{Array, Int, Map};

    #[test]
    fn test_decode_array_without_size() {
        let mut input: &[u8] = &[6, 2, 4, 6, 0];
        let result = decode(&Schema::Array(Box::new(Schema::Int)), &mut input);
        assert_eq!(Array(vec!(Int(1), Int(2), Int(3))), result.unwrap());
    }

    #[test]
    fn test_decode_array_with_size() {
        let mut input: &[u8] = &[5, 6, 2, 4, 6, 0];
        let result = decode(&Schema::Array(Box::new(Schema::Int)), &mut input);
        assert_eq!(Array(vec!(Int(1), Int(2), Int(3))), result.unwrap());
    }

    #[test]
    fn test_decode_map_without_size() {
        let mut input: &[u8] = &[0x02, 0x08, 0x74, 0x65, 0x73, 0x74, 0x02, 0x00];
        let result = decode(&Schema::Map(Box::new(Schema::Int)), &mut input);
        let mut expected = HashMap::new();
        expected.insert(String::from("test"), Int(1));
        assert_eq!(Map(expected), result.unwrap());
    }

    #[test]
    fn test_decode_map_with_size() {
        let mut input: &[u8] = &[0x01, 0x0C, 0x08, 0x74, 0x65, 0x73, 0x74, 0x02, 0x00];
        let result = decode(&Schema::Map(Box::new(Schema::Int)), &mut input);
        let mut expected = HashMap::new();
        expected.insert(String::from("test"), Int(1));
        assert_eq!(Map(expected), result.unwrap());
    }

    #[test]
    fn test_negative_decimal_value() {
        use crate::{encode::encode, schema::Name};
        use num_bigint::ToBigInt;
        let inner = Box::new(Schema::Fixed {
            size: 2,
            name: Name::new("decimal"),
        });
        let schema = Schema::Decimal {
            inner,
            precision: 4,
            scale: 2,
        };
        let bigint = -423.to_bigint().unwrap();
        let value = Value::Decimal(Decimal::from(bigint.to_signed_bytes_be()));

        let mut buffer = Vec::new();
        encode(&value, &schema, &mut buffer);

        let mut bytes = &buffer[..];
        let result = decode(&schema, &mut bytes).unwrap();
        assert_eq!(result, value);
    }

    #[test]
    fn test_decode_decimal_with_bigger_than_necessary_size() {
        use crate::{encode::encode, schema::Name};
        use num_bigint::ToBigInt;
        let inner = Box::new(Schema::Fixed {
            size: 13,
            name: Name::new("decimal"),
        });
        let schema = Schema::Decimal {
            inner,
            precision: 4,
            scale: 2,
        };
        let value = Value::Decimal(Decimal::from(
            (-423.to_bigint().unwrap()).to_signed_bytes_be(),
        ));
        let mut buffer = Vec::<u8>::new();

        encode(&value, &schema, &mut buffer);
        let mut bytes: &[u8] = &buffer[..];
        let result = decode(&schema, &mut bytes).unwrap();
        assert_eq!(result, value);
    }
}

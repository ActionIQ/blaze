use bigdecimal::ToPrimitive;
use datafusion::arrow::array::*;
use datafusion::arrow::datatypes::*;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::{Result, ScalarValue};
use datafusion::logical_expr::ColumnarValue;
use datafusion::physical_expr::PhysicalExpr;
use num::{Bounded, FromPrimitive, Integer, Signed};
use paste::paste;
use std::any::Any;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::Arc;

/// cast expression compatible with spark
#[derive(Debug)]
pub struct TryCastExpr {
    pub expr: Arc<dyn PhysicalExpr>,
    pub cast_type: DataType,
}

impl TryCastExpr {
    pub fn new(expr: Arc<dyn PhysicalExpr>, cast_type: DataType) -> Self {
        Self { expr, cast_type }
    }
}

impl Display for TryCastExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "cast({} AS {:?})", self.expr, self.cast_type)
    }
}

impl PhysicalExpr for TryCastExpr {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(self.cast_type.clone())
    }

    fn nullable(&self, _input_schema: &Schema) -> Result<bool> {
        Ok(true)
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let value = self.expr.evaluate(batch)?;
        match (&value.data_type(), &self.cast_type) {
            (&DataType::Utf8, &DataType::Int8)
            | (&DataType::Utf8, &DataType::Int16)
            | (&DataType::Utf8, &DataType::Int32)
            | (&DataType::Utf8, &DataType::Int64) => {
                // spark compatible string to integer cast
                match value {
                    ColumnarValue::Array(array) => Ok(ColumnarValue::Array(
                        try_cast_string_array_to_integer(&array, &self.cast_type)?,
                    )),
                    ColumnarValue::Scalar(scalar) => {
                        let scalar_array = scalar.to_array();
                        let cast_array = try_cast_string_array_to_integer(
                            &scalar_array,
                            &self.cast_type,
                        )?;
                        let cast_scalar = ScalarValue::try_from_array(&cast_array, 0)?;
                        Ok(ColumnarValue::Scalar(cast_scalar))
                    }
                }
            }
            (&DataType::Utf8, &DataType::Decimal128(_, _)) => {
                // spark compatible string to decimal cast
                match value {
                    ColumnarValue::Array(array) => Ok(ColumnarValue::Array(
                        try_cast_string_array_to_decimal(&array, &self.cast_type)?,
                    )),
                    ColumnarValue::Scalar(scalar) => {
                        let scalar_array = scalar.to_array();
                        let cast_array = try_cast_string_array_to_decimal(
                            &scalar_array,
                            &self.cast_type,
                        )?;
                        let cast_scalar = ScalarValue::try_from_array(&cast_array, 0)?;
                        Ok(ColumnarValue::Scalar(cast_scalar))
                    }
                }
            }
            _ => {
                // default cast
                match value {
                    ColumnarValue::Array(array) => Ok(ColumnarValue::Array(
                        datafusion::arrow::compute::kernels::cast::cast(
                            &array,
                            &self.cast_type,
                        )?,
                    )),
                    ColumnarValue::Scalar(scalar) => {
                        let scalar_array = scalar.to_array();
                        let cast_array = datafusion::arrow::compute::kernels::cast::cast(
                            &scalar_array,
                            &self.cast_type,
                        )?;
                        let cast_scalar = ScalarValue::try_from_array(&cast_array, 0)?;
                        Ok(ColumnarValue::Scalar(cast_scalar))
                    }
                }
            }
        }
    }
}

fn try_cast_string_array_to_integer(
    array: &ArrayRef,
    cast_type: &DataType,
) -> Result<ArrayRef> {
    macro_rules! cast {
        ($target_type:ident) => {{
            type B = paste! {[<$target_type Builder>]};
            let array = array.as_any().downcast_ref::<StringArray>().unwrap();
            let mut builder = B::new();

            for v in array.iter() {
                match v {
                    Some(s) => builder.append_option(to_integer(s)),
                    None => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }};
    }

    Ok(match cast_type {
        DataType::Int8 => cast!(Int8),
        DataType::Int16 => cast!(Int16),
        DataType::Int32 => cast!(Int32),
        DataType::Int64 => cast!(Int64),
        _ => datafusion::arrow::compute::cast(array, cast_type)?,
    })
}

fn try_cast_string_array_to_decimal(
    array: &ArrayRef,
    cast_type: &DataType,
) -> Result<ArrayRef> {
    if let &DataType::Decimal128(precision, scale) = cast_type {
        let array = array.as_any().downcast_ref::<StringArray>().unwrap();
        let mut builder = Decimal128Builder::new(precision, scale);

        for v in array.iter() {
            match v {
                Some(s) => match to_decimal(s, precision, scale) {
                    Some(v) => builder.append_value(v)?,
                    None => builder.append_null(),
                },
                None => builder.append_null(),
            }
        }
        return Ok(Arc::new(builder.finish()));
    }
    unreachable!("cast_type must be DataType::Decimal")
}

// this implementation is original copied from spark UTF8String.scala
fn to_integer<T: Bounded + FromPrimitive + Integer + Signed + Copy>(
    input: &str,
) -> Option<T> {
    let bytes = input.as_bytes();

    if bytes.is_empty() {
        return None;
    }

    let b = bytes[0];
    let negative = b == b'-';
    let mut offset = 0;

    if negative || b == b'+' {
        offset += 1;
        if bytes.len() == 1 {
            return None;
        }
    }

    let separator = b'.';
    let radix = T::from_usize(10).unwrap();
    let stop_value = T::min_value() / radix;
    let mut result = T::zero();

    while offset < bytes.len() {
        let b = bytes[offset];
        offset += 1;
        if b == separator {
            // We allow decimals and will return a truncated integral in that case.
            // Therefore we won't throw an exception here (checking the fractional
            // part happens below.)
            break;
        }

        let digit;
        if (b'0'..=b'9').contains(&b) {
            digit = b - b'0';
        } else {
            return None;
        }

        // We are going to process the new digit and accumulate the result. However, before doing
        // this, if the result is already smaller than the stopValue(Long.MIN_VALUE / radix), then
        // result * 10 will definitely be smaller than minValue, and we can stop.
        if result < stop_value {
            return None;
        }

        result = result * radix - T::from_u8(digit).unwrap();
        // Since the previous result is less than or equal to stopValue(Long.MIN_VALUE / radix), we
        // can just use `result > 0` to check overflow. If result overflows, we should stop.
        if result > T::zero() {
            return None;
        }
    }

    // This is the case when we've encountered a decimal separator. The fractional
    // part will not change the number, but we will verify that the fractional part
    // is well formed.
    while offset < bytes.len() {
        let current_byte = bytes[offset];
        if !(b'0'..=b'9').contains(&current_byte) {
            return None;
        }
        offset += 1;
    }

    if !negative {
        result = -result;
        if result < T::zero() {
            return None;
        }
    }
    Some(result)
}

fn to_decimal(input: &str, precision: u8, scale: u8) -> Option<i128> {
    let precision = precision as u64;
    let scale = scale as i64;
    bigdecimal::BigDecimal::from_str(input)
        .ok()
        .map(|decimal| decimal.with_prec(precision).with_scale(scale))
        .and_then(|decimal| {
            let (bigint, _exp) = decimal.as_bigint_and_exponent();
            bigint.to_i128()
        })
}

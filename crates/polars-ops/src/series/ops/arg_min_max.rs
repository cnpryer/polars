use argminmax::ArgMinMax;
use arrow::array::Array;
use polars_core::chunked_array::ops::float_sorted_arg_max::{
    float_arg_max_sorted_ascending, float_arg_max_sorted_descending,
};
use polars_core::series::IsSorted;
use polars_core::with_match_categorical_physical_type;

use super::*;

/// Argmin/ Argmax
pub trait ArgAgg {
    /// Get the index of the minimal value
    fn arg_min(&self) -> Option<usize>;
    /// Get the index of the maximal value
    fn arg_max(&self) -> Option<usize>;
}

macro_rules! with_match_physical_numeric_polars_type {(
    $key_type:expr, | $_:tt $T:ident | $($body:tt)*
) => ({
    macro_rules! __with_ty__ {( $_ $T:ident ) => ( $($body)* )}
    use DataType::*;
    match $key_type {
            #[cfg(feature = "dtype-i8")]
        Int8 => __with_ty__! { Int8Type },
            #[cfg(feature = "dtype-i16")]
        Int16 => __with_ty__! { Int16Type },
        Int32 => __with_ty__! { Int32Type },
        Int64 => __with_ty__! { Int64Type },
            #[cfg(feature = "dtype-u8")]
        UInt8 => __with_ty__! { UInt8Type },
            #[cfg(feature = "dtype-u16")]
        UInt16 => __with_ty__! { UInt16Type },
        UInt32 => __with_ty__! { UInt32Type },
        UInt64 => __with_ty__! { UInt64Type },
        Float32 => __with_ty__! { Float32Type },
        Float64 => __with_ty__! { Float64Type },
        dt => panic!("not implemented for dtype {:?}", dt),
    }
})}

impl ArgAgg for Series {
    fn arg_min(&self) -> Option<usize> {
        use DataType::*;
        let phys_s = self.to_physical_repr();
        match self.dtype() {
            #[cfg(feature = "dtype-categorical")]
            Categorical(cats, _) => {
                with_match_categorical_physical_type!(cats.physical(), |$C| {
                    let ca = self.cat::<$C>().unwrap();
                    if ca.null_count() == ca.len() {
                        return None;
                    }
                    ca.iter_str()
                        .enumerate()
                        .flat_map(|(idx, val)| val.map(|val| (idx, val)))
                        .reduce(|acc, (idx, val)| if acc.1 > val { (idx, val) } else { acc })
                        .map(|tpl| tpl.0)
                })
            },
            #[cfg(feature = "dtype-categorical")]
            Enum(_, _) => phys_s.arg_min(),
            Date | Datetime(_, _) | Duration(_) | Time => phys_s.arg_min(),
            String => {
                let ca = self.str().unwrap();
                arg_min_str(ca)
            },
            Boolean => {
                let ca = self.bool().unwrap();
                arg_min_bool(ca)
            },
            dt if dt.is_primitive_numeric() => {
                with_match_physical_numeric_polars_type!(phys_s.dtype(), |$T| {
                    let ca: &ChunkedArray<$T> = phys_s.as_ref().as_ref().as_ref();
                    arg_min_numeric_dispatch(ca)
                })
            },
            _ => None,
        }
    }

    fn arg_max(&self) -> Option<usize> {
        use DataType::*;
        let phys_s = self.to_physical_repr();
        match self.dtype() {
            #[cfg(feature = "dtype-categorical")]
            Categorical(cats, _) => {
                with_match_categorical_physical_type!(cats.physical(), |$C| {
                    let ca = self.cat::<$C>().unwrap();
                    if ca.null_count() == ca.len() {
                        return None;
                    }
                    ca.iter_str()
                        .enumerate()
                        .flat_map(|(idx, val)| val.map(|val| (idx, val)))
                        .reduce(|acc, (idx, val)| if acc.1 < val { (idx, val) } else { acc })
                        .map(|tpl| tpl.0)
                })
            },
            #[cfg(feature = "dtype-categorical")]
            Enum(_, _) => phys_s.arg_max(),
            Date | Datetime(_, _) | Duration(_) | Time => phys_s.arg_max(),
            String => {
                let ca = self.str().unwrap();
                arg_max_str(ca)
            },
            Boolean => {
                let ca = self.bool().unwrap();
                arg_max_bool(ca)
            },
            dt if dt.is_primitive_numeric() => {
                with_match_physical_numeric_polars_type!(phys_s.dtype(), |$T| {
                    let ca: &ChunkedArray<$T> = phys_s.as_ref().as_ref().as_ref();
                    arg_max_numeric_dispatch(ca)
                })
            },
            _ => None,
        }
    }
}

fn arg_max_numeric_dispatch<T>(ca: &ChunkedArray<T>) -> Option<usize>
where
    T: PolarsNumericType,
    for<'b> &'b [T::Native]: ArgMinMax,
{
    if ca.null_count() == ca.len() {
        None
    } else if T::get_static_dtype().is_float() && !matches!(ca.is_sorted_flag(), IsSorted::Not) {
        arg_max_float_sorted(ca)
    } else if let Ok(vals) = ca.cont_slice() {
        arg_max_numeric_slice(vals, ca.is_sorted_flag())
    } else {
        arg_max_numeric(ca)
    }
}

fn arg_min_numeric_dispatch<T>(ca: &ChunkedArray<T>) -> Option<usize>
where
    T: PolarsNumericType,
    for<'b> &'b [T::Native]: ArgMinMax,
{
    if ca.null_count() == ca.len() {
        None
    } else if let Ok(vals) = ca.cont_slice() {
        arg_min_numeric_slice(vals, ca.is_sorted_flag())
    } else {
        arg_min_numeric(ca)
    }
}

fn arg_max_bool(ca: &BooleanChunked) -> Option<usize> {
    ca.first_true_idx().or_else(|| ca.first_false_idx())
}

/// # Safety
/// `ca` has a float dtype, has at least one non-null value and is sorted.
fn arg_max_float_sorted<T>(ca: &ChunkedArray<T>) -> Option<usize>
where
    T: PolarsNumericType,
{
    let out = match ca.is_sorted_flag() {
        IsSorted::Ascending => float_arg_max_sorted_ascending(ca),
        IsSorted::Descending => float_arg_max_sorted_descending(ca),
        _ => unreachable!(),
    };
    Some(out)
}

fn arg_min_bool(ca: &BooleanChunked) -> Option<usize> {
    ca.first_false_idx().or_else(|| ca.first_true_idx())
}

fn arg_min_str(ca: &StringChunked) -> Option<usize> {
    if ca.null_count() == ca.len() {
        return None;
    }
    match ca.is_sorted_flag() {
        IsSorted::Ascending => ca.first_non_null(),
        IsSorted::Descending => ca.last_non_null(),
        IsSorted::Not => ca
            .iter()
            .enumerate()
            .flat_map(|(idx, val)| val.map(|val| (idx, val)))
            .reduce(|acc, (idx, val)| if acc.1 > val { (idx, val) } else { acc })
            .map(|tpl| tpl.0),
    }
}

fn arg_max_str(ca: &StringChunked) -> Option<usize> {
    if ca.null_count() == ca.len() {
        return None;
    }
    match ca.is_sorted_flag() {
        IsSorted::Ascending => ca.last_non_null(),
        IsSorted::Descending => ca.first_non_null(),
        IsSorted::Not => ca
            .iter()
            .enumerate()
            .reduce(|acc, (idx, val)| if acc.1 < val { (idx, val) } else { acc })
            .map(|tpl| tpl.0),
    }
}

fn arg_min_numeric<'a, T>(ca: &'a ChunkedArray<T>) -> Option<usize>
where
    T: PolarsNumericType,
    for<'b> &'b [T::Native]: ArgMinMax,
{
    match ca.is_sorted_flag() {
        IsSorted::Ascending => ca.first_non_null(),
        IsSorted::Descending => ca.last_non_null(),
        IsSorted::Not => {
            ca.downcast_iter()
                .fold((None, None, 0), |acc, arr| {
                    if arr.len() == 0 {
                        return acc;
                    }
                    let chunk_min: Option<(usize, T::Native)> = if arr.null_count() > 0 {
                        arr.into_iter()
                            .enumerate()
                            .flat_map(|(idx, val)| val.map(|val| (idx, *val)))
                            .reduce(|acc, (idx, val)| if acc.1 > val { (idx, val) } else { acc })
                    } else {
                        // When no nulls & array not empty => we can use fast argminmax
                        let min_idx: usize = arr.values().as_slice().argmin();
                        Some((min_idx, arr.value(min_idx)))
                    };

                    let new_offset: usize = acc.2 + arr.len();
                    match acc {
                        (Some(_), Some(acc_v), offset) => match chunk_min {
                            Some((idx, val)) if val < acc_v => {
                                (Some(idx + offset), Some(val), new_offset)
                            },
                            _ => (acc.0, acc.1, new_offset),
                        },
                        (None, None, offset) => match chunk_min {
                            Some((idx, val)) => (Some(idx + offset), Some(val), new_offset),
                            None => (None, None, new_offset),
                        },
                        _ => unreachable!(),
                    }
                })
                .0
        },
    }
}

fn arg_max_numeric<'a, T>(ca: &'a ChunkedArray<T>) -> Option<usize>
where
    T: PolarsNumericType,
    for<'b> &'b [T::Native]: ArgMinMax,
{
    match ca.is_sorted_flag() {
        IsSorted::Ascending => ca.last_non_null(),
        IsSorted::Descending => ca.first_non_null(),
        IsSorted::Not => {
            ca.downcast_iter()
                .fold((None, None, 0), |acc, arr| {
                    if arr.len() == 0 {
                        return acc;
                    }
                    let chunk_max: Option<(usize, T::Native)> = if arr.null_count() > 0 {
                        // When there are nulls, we should compare Option<T::Native>
                        arr.into_iter()
                            .enumerate()
                            .flat_map(|(idx, val)| val.map(|val| (idx, *val)))
                            .reduce(|acc, (idx, val)| if acc.1 < val { (idx, val) } else { acc })
                    } else {
                        // When no nulls & array not empty => we can use fast argminmax
                        let max_idx: usize = arr.values().as_slice().argmax();
                        Some((max_idx, arr.value(max_idx)))
                    };

                    let new_offset: usize = acc.2 + arr.len();
                    match acc {
                        (Some(_), Some(acc_v), offset) => match chunk_max {
                            Some((idx, val)) if acc_v < val => {
                                (Some(idx + offset), Some(val), new_offset)
                            },
                            _ => (acc.0, acc.1, new_offset),
                        },
                        (None, None, offset) => match chunk_max {
                            Some((idx, val)) => (Some(idx + offset), Some(val), new_offset),
                            None => (None, None, new_offset),
                        },
                        _ => unreachable!(),
                    }
                })
                .0
        },
    }
}

fn arg_min_numeric_slice<T>(vals: &[T], is_sorted: IsSorted) -> Option<usize>
where
    for<'a> &'a [T]: ArgMinMax,
{
    match is_sorted {
        // all vals are not null guarded by cont_slice
        IsSorted::Ascending => Some(0),
        // all vals are not null guarded by cont_slice
        IsSorted::Descending => Some(vals.len() - 1),
        IsSorted::Not => Some(vals.argmin()), // assumes not empty
    }
}

fn arg_max_numeric_slice<T>(vals: &[T], is_sorted: IsSorted) -> Option<usize>
where
    for<'a> &'a [T]: ArgMinMax,
{
    match is_sorted {
        // all vals are not null guarded by cont_slice
        IsSorted::Ascending => Some(vals.len() - 1),
        // all vals are not null guarded by cont_slice
        IsSorted::Descending => Some(0),
        IsSorted::Not => Some(vals.argmax()), // assumes not empty
    }
}

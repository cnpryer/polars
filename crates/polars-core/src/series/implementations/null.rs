use std::any::Any;

use polars_error::constants::LENGTH_LIMIT_MSG;

use self::compare_inner::TotalOrdInner;
use super::*;
use crate::chunked_array::ops::compare_inner::{IntoTotalEqInner, NonNull, TotalEqInner};
use crate::chunked_array::ops::sort::arg_sort_multiple::arg_sort_multiple_impl;
use crate::prelude::*;
use crate::series::private::{PrivateSeries, PrivateSeriesNumeric};
use crate::series::*;

impl Series {
    pub fn new_null(name: PlSmallStr, len: usize) -> Series {
        NullChunked::new(name, len).into_series()
    }
}

#[derive(Clone)]
pub struct NullChunked {
    pub(crate) name: PlSmallStr,
    length: IdxSize,
    // we still need chunks as many series consumers expect
    // chunks to be there
    chunks: Vec<ArrayRef>,
}

impl NullChunked {
    pub(crate) fn new(name: PlSmallStr, len: usize) -> Self {
        Self {
            name,
            length: len as IdxSize,
            chunks: vec![Box::new(arrow::array::NullArray::new(
                ArrowDataType::Null,
                len,
            ))],
        }
    }

    pub fn len(&self) -> usize {
        self.length as usize
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}
impl PrivateSeriesNumeric for NullChunked {
    fn bit_repr(&self) -> Option<BitRepr> {
        Some(BitRepr::U32(UInt32Chunked::full_null(
            self.name.clone(),
            self.len(),
        )))
    }
}

impl PrivateSeries for NullChunked {
    fn compute_len(&mut self) {
        fn inner(chunks: &[ArrayRef]) -> usize {
            match chunks.len() {
                // fast path
                1 => chunks[0].len(),
                _ => chunks.iter().fold(0, |acc, arr| acc + arr.len()),
            }
        }
        self.length = IdxSize::try_from(inner(&self.chunks)).expect(LENGTH_LIMIT_MSG);
    }
    fn _field(&self) -> Cow<'_, Field> {
        Cow::Owned(Field::new(self.name().clone(), DataType::Null))
    }

    #[allow(unused)]
    fn _set_flags(&mut self, flags: StatisticsFlags) {}

    fn _dtype(&self) -> &DataType {
        &DataType::Null
    }

    #[cfg(feature = "zip_with")]
    fn zip_with_same_type(&self, mask: &BooleanChunked, other: &Series) -> PolarsResult<Series> {
        let len = match (self.len(), mask.len(), other.len()) {
            (a, b, c) if a == b && b == c => a,
            (1, a, b) | (a, 1, b) | (a, b, 1) if a == b => a,
            (a, 1, 1) | (1, a, 1) | (1, 1, a) => a,
            (_, 0, _) => 0,
            _ => {
                polars_bail!(ShapeMismatch: "shapes of `self`, `mask` and `other` are not suitable for `zip_with` operation")
            },
        };

        Ok(Self::new(self.name().clone(), len).into_series())
    }

    fn into_total_eq_inner<'a>(&'a self) -> Box<dyn TotalEqInner + 'a> {
        IntoTotalEqInner::into_total_eq_inner(self)
    }
    fn into_total_ord_inner<'a>(&'a self) -> Box<dyn TotalOrdInner + 'a> {
        IntoTotalOrdInner::into_total_ord_inner(self)
    }

    fn subtract(&self, _rhs: &Series) -> PolarsResult<Series> {
        null_arithmetic(self, _rhs, "subtract")
    }

    fn add_to(&self, _rhs: &Series) -> PolarsResult<Series> {
        null_arithmetic(self, _rhs, "add_to")
    }
    fn multiply(&self, _rhs: &Series) -> PolarsResult<Series> {
        null_arithmetic(self, _rhs, "multiply")
    }
    fn divide(&self, _rhs: &Series) -> PolarsResult<Series> {
        null_arithmetic(self, _rhs, "divide")
    }
    fn remainder(&self, _rhs: &Series) -> PolarsResult<Series> {
        null_arithmetic(self, _rhs, "remainder")
    }

    #[cfg(feature = "algorithm_group_by")]
    fn group_tuples(&self, _multithreaded: bool, _sorted: bool) -> PolarsResult<GroupsType> {
        Ok(if self.is_empty() {
            GroupsType::default()
        } else {
            GroupsType::Slice {
                groups: vec![[0, self.length]],
                rolling: false,
            }
        })
    }

    #[cfg(feature = "algorithm_group_by")]
    unsafe fn agg_list(&self, groups: &GroupsType) -> Series {
        AggList::agg_list(self, groups)
    }

    fn _get_flags(&self) -> StatisticsFlags {
        StatisticsFlags::empty()
    }

    fn vec_hash(
        &self,
        random_state: PlSeedableRandomStateQuality,
        buf: &mut Vec<u64>,
    ) -> PolarsResult<()> {
        VecHash::vec_hash(self, random_state, buf)?;
        Ok(())
    }

    fn vec_hash_combine(
        &self,
        build_hasher: PlSeedableRandomStateQuality,
        hashes: &mut [u64],
    ) -> PolarsResult<()> {
        VecHash::vec_hash_combine(self, build_hasher, hashes)?;
        Ok(())
    }

    fn arg_sort_multiple(
        &self,
        by: &[Column],
        options: &SortMultipleOptions,
    ) -> PolarsResult<IdxCa> {
        let vals = (0..self.len())
            .map(|i| (i as IdxSize, NonNull(())))
            .collect();
        arg_sort_multiple_impl(vals, by, options)
    }
}

fn null_arithmetic(lhs: &NullChunked, rhs: &Series, op: &str) -> PolarsResult<Series> {
    let output_len = match (lhs.len(), rhs.len()) {
        (1, len_r) => len_r,
        (len_l, 1) => len_l,
        (len_l, len_r) if len_l == len_r => len_l,
        _ => polars_bail!(ComputeError: "Cannot {:?} two series of different lengths.", op),
    };
    Ok(NullChunked::new(lhs.name().clone(), output_len).into_series())
}

impl SeriesTrait for NullChunked {
    fn name(&self) -> &PlSmallStr {
        &self.name
    }

    fn rename(&mut self, name: PlSmallStr) {
        self.name = name
    }

    fn chunks(&self) -> &Vec<ArrayRef> {
        &self.chunks
    }
    unsafe fn chunks_mut(&mut self) -> &mut Vec<ArrayRef> {
        &mut self.chunks
    }

    fn chunk_lengths(&self) -> ChunkLenIter<'_> {
        self.chunks.iter().map(|chunk| chunk.len())
    }

    fn take(&self, indices: &IdxCa) -> PolarsResult<Series> {
        Ok(NullChunked::new(self.name.clone(), indices.len()).into_series())
    }

    unsafe fn take_unchecked(&self, indices: &IdxCa) -> Series {
        NullChunked::new(self.name.clone(), indices.len()).into_series()
    }

    fn take_slice(&self, indices: &[IdxSize]) -> PolarsResult<Series> {
        Ok(NullChunked::new(self.name.clone(), indices.len()).into_series())
    }

    unsafe fn take_slice_unchecked(&self, indices: &[IdxSize]) -> Series {
        NullChunked::new(self.name.clone(), indices.len()).into_series()
    }

    fn len(&self) -> usize {
        self.length as usize
    }

    fn has_nulls(&self) -> bool {
        !self.is_empty()
    }

    fn rechunk(&self) -> Series {
        NullChunked::new(self.name.clone(), self.len()).into_series()
    }

    fn drop_nulls(&self) -> Series {
        NullChunked::new(self.name.clone(), 0).into_series()
    }

    fn cast(&self, dtype: &DataType, _cast_options: CastOptions) -> PolarsResult<Series> {
        Ok(Series::full_null(self.name.clone(), self.len(), dtype))
    }

    fn null_count(&self) -> usize {
        self.len()
    }

    #[cfg(feature = "algorithm_group_by")]
    fn unique(&self) -> PolarsResult<Series> {
        let ca = NullChunked::new(self.name.clone(), self.n_unique().unwrap());
        Ok(ca.into_series())
    }

    #[cfg(feature = "algorithm_group_by")]
    fn n_unique(&self) -> PolarsResult<usize> {
        let n = if self.is_empty() { 0 } else { 1 };
        Ok(n)
    }

    #[cfg(feature = "algorithm_group_by")]
    fn arg_unique(&self) -> PolarsResult<IdxCa> {
        let idxs: Vec<IdxSize> = (0..self.n_unique().unwrap() as IdxSize).collect();
        Ok(IdxCa::new(self.name().clone(), idxs))
    }

    fn new_from_index(&self, _index: usize, length: usize) -> Series {
        NullChunked::new(self.name.clone(), length).into_series()
    }

    unsafe fn get_unchecked(&self, _index: usize) -> AnyValue<'_> {
        AnyValue::Null
    }

    fn slice(&self, offset: i64, length: usize) -> Series {
        let (chunks, len) = chunkops::slice(&self.chunks, offset, length, self.len());
        NullChunked {
            name: self.name.clone(),
            length: len as IdxSize,
            chunks,
        }
        .into_series()
    }

    fn split_at(&self, offset: i64) -> (Series, Series) {
        let (l, r) = chunkops::split_at(self.chunks(), offset, self.len());
        (
            NullChunked {
                name: self.name.clone(),
                length: l.iter().map(|arr| arr.len() as IdxSize).sum(),
                chunks: l,
            }
            .into_series(),
            NullChunked {
                name: self.name.clone(),
                length: r.iter().map(|arr| arr.len() as IdxSize).sum(),
                chunks: r,
            }
            .into_series(),
        )
    }

    fn sort_with(&self, _options: SortOptions) -> PolarsResult<Series> {
        Ok(self.clone().into_series())
    }

    fn arg_sort(&self, _options: SortOptions) -> IdxCa {
        IdxCa::from_vec(self.name().clone(), (0..self.len() as IdxSize).collect())
    }

    fn is_null(&self) -> BooleanChunked {
        BooleanChunked::full(self.name().clone(), true, self.len())
    }

    fn is_not_null(&self) -> BooleanChunked {
        BooleanChunked::full(self.name().clone(), false, self.len())
    }

    fn reverse(&self) -> Series {
        self.clone().into_series()
    }

    fn filter(&self, filter: &BooleanChunked) -> PolarsResult<Series> {
        let len = if self.is_empty() {
            // We still allow a length of `1` because it could be `lit(true)`.
            polars_ensure!(filter.len() <= 1, ShapeMismatch: "filter's length: {} differs from that of the series: 0", filter.len());
            0
        } else if filter.len() == 1 {
            return match filter.get(0) {
                Some(true) => Ok(self.clone().into_series()),
                None | Some(false) => Ok(NullChunked::new(self.name.clone(), 0).into_series()),
            };
        } else {
            polars_ensure!(filter.len() == self.len(), ShapeMismatch: "filter's length: {} differs from that of the series: {}", filter.len(), self.len());
            filter.sum().unwrap_or(0) as usize
        };
        Ok(NullChunked::new(self.name.clone(), len).into_series())
    }

    fn shift(&self, _periods: i64) -> Series {
        self.clone().into_series()
    }

    fn append(&mut self, other: &Series) -> PolarsResult<()> {
        polars_ensure!(other.dtype() == &DataType::Null, ComputeError: "expected null dtype");
        // we don't create a new null array to keep probability of aligned chunks higher
        self.length += other.len() as IdxSize;
        self.chunks.extend(other.chunks().iter().cloned());
        Ok(())
    }
    fn append_owned(&mut self, mut other: Series) -> PolarsResult<()> {
        polars_ensure!(other.dtype() == &DataType::Null, ComputeError: "expected null dtype");
        // we don't create a new null array to keep probability of aligned chunks higher
        let other: &mut NullChunked = other._get_inner_mut().as_any_mut().downcast_mut().unwrap();
        self.length += other.len() as IdxSize;
        self.chunks.extend(std::mem::take(&mut other.chunks));
        Ok(())
    }

    fn extend(&mut self, other: &Series) -> PolarsResult<()> {
        *self = NullChunked::new(self.name.clone(), self.len() + other.len());
        Ok(())
    }

    fn clone_inner(&self) -> Arc<dyn SeriesTrait> {
        Arc::new(self.clone())
    }

    fn find_validity_mismatch(&self, other: &Series, idxs: &mut Vec<IdxSize>) {
        ChunkNestingUtils::find_validity_mismatch(self, other, idxs)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_phys_any(&self) -> &dyn Any {
        self
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self as _
    }
}

unsafe impl IntoSeries for NullChunked {
    fn into_series(self) -> Series
    where
        Self: Sized,
    {
        Series(Arc::new(self))
    }
}

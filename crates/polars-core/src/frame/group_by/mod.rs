use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;

use num_traits::NumCast;
use polars_compute::rolling::QuantileMethod;
use polars_utils::format_pl_smallstr;
use polars_utils::hashing::DirtyHash;
use rayon::prelude::*;

use self::hashing::*;
use crate::POOL;
use crate::prelude::*;
use crate::utils::{_set_partition_size, accumulate_dataframes_vertical};

pub mod aggregations;
pub mod expr;
pub(crate) mod hashing;
mod into_groups;
mod position;

pub use into_groups::*;
pub use position::*;

use crate::chunked_array::ops::row_encode::{
    encode_rows_unordered, encode_rows_vertical_par_unordered,
};

impl DataFrame {
    pub fn group_by_with_series(
        &self,
        mut by: Vec<Column>,
        multithreaded: bool,
        sorted: bool,
    ) -> PolarsResult<GroupBy<'_>> {
        polars_ensure!(
            !by.is_empty(),
            ComputeError: "at least one key is required in a group_by operation"
        );

        // Ensure all 'by' columns have the same common_height
        // The condition self.width > 0 ensures we can still call this on a
        // dummy dataframe where we provide the keys
        let common_height = if self.width() > 0 {
            self.height()
        } else {
            by.iter().map(|s| s.len()).max().expect("at least 1 key")
        };
        for by_key in by.iter_mut() {
            if by_key.len() != common_height {
                polars_ensure!(
                    by_key.len() == 1,
                    ShapeMismatch: "series used as keys should have the same length as the DataFrame"
                );
                *by_key = by_key.new_from_index(0, common_height)
            }
        }

        let groups = if by.len() == 1 {
            let column = &by[0];
            column
                .as_materialized_series()
                .group_tuples(multithreaded, sorted)
        } else if by.iter().any(|s| s.dtype().is_object()) {
            #[cfg(feature = "object")]
            {
                let mut df = DataFrame::new(by.clone()).unwrap();
                let n = df.height();
                let rows = df.to_av_rows();
                let iter = (0..n).map(|i| rows.get(i));
                Ok(group_by(iter, sorted))
            }
            #[cfg(not(feature = "object"))]
            {
                unreachable!()
            }
        } else {
            // Skip null dtype.
            let by = by
                .iter()
                .filter(|s| !s.dtype().is_null())
                .cloned()
                .collect::<Vec<_>>();
            if by.is_empty() {
                let groups = if self.is_empty() {
                    vec![]
                } else {
                    vec![[0, self.height() as IdxSize]]
                };
                Ok(GroupsType::Slice {
                    groups,
                    rolling: false,
                })
            } else {
                let rows = if multithreaded {
                    encode_rows_vertical_par_unordered(&by)
                } else {
                    encode_rows_unordered(&by)
                }?
                .into_series();
                rows.group_tuples(multithreaded, sorted)
            }
        };
        Ok(GroupBy::new(self, by, groups?.into_sliceable(), None))
    }

    /// Group DataFrame using a Series column.
    ///
    /// # Example
    ///
    /// ```
    /// use polars_core::prelude::*;
    /// fn group_by_sum(df: &DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["column_name"])?
    ///     .select(["agg_column_name"])
    ///     .sum()
    /// }
    /// ```
    pub fn group_by<I, S>(&self, by: I) -> PolarsResult<GroupBy<'_>>
    where
        I: IntoIterator<Item = S>,
        S: Into<PlSmallStr>,
    {
        let selected_keys = self.select_columns(by)?;
        self.group_by_with_series(selected_keys, true, false)
    }

    /// Group DataFrame using a Series column.
    /// The groups are ordered by their smallest row index.
    pub fn group_by_stable<I, S>(&self, by: I) -> PolarsResult<GroupBy<'_>>
    where
        I: IntoIterator<Item = S>,
        S: Into<PlSmallStr>,
    {
        let selected_keys = self.select_columns(by)?;
        self.group_by_with_series(selected_keys, true, true)
    }
}

/// Returned by a group_by operation on a DataFrame. This struct supports
/// several aggregations.
///
/// Until described otherwise, the examples in this struct are performed on the following DataFrame:
///
/// ```ignore
/// use polars_core::prelude::*;
///
/// let dates = &[
/// "2020-08-21",
/// "2020-08-21",
/// "2020-08-22",
/// "2020-08-23",
/// "2020-08-22",
/// ];
/// // date format
/// let fmt = "%Y-%m-%d";
/// // create date series
/// let s0 = DateChunked::parse_from_str_slice("date", dates, fmt)
///         .into_series();
/// // create temperature series
/// let s1 = Series::new("temp".into(), [20, 10, 7, 9, 1]);
/// // create rain series
/// let s2 = Series::new("rain".into(), [0.2, 0.1, 0.3, 0.1, 0.01]);
/// // create a new DataFrame
/// let df = DataFrame::new(vec![s0, s1, s2]).unwrap();
/// println!("{:?}", df);
/// ```
///
/// Outputs:
///
/// ```text
/// +------------+------+------+
/// | date       | temp | rain |
/// | ---        | ---  | ---  |
/// | Date       | i32  | f64  |
/// +============+======+======+
/// | 2020-08-21 | 20   | 0.2  |
/// +------------+------+------+
/// | 2020-08-21 | 10   | 0.1  |
/// +------------+------+------+
/// | 2020-08-22 | 7    | 0.3  |
/// +------------+------+------+
/// | 2020-08-23 | 9    | 0.1  |
/// +------------+------+------+
/// | 2020-08-22 | 1    | 0.01 |
/// +------------+------+------+
/// ```
///
#[derive(Debug, Clone)]
pub struct GroupBy<'a> {
    pub df: &'a DataFrame,
    pub(crate) selected_keys: Vec<Column>,
    // [first idx, [other idx]]
    groups: GroupPositions,
    // columns selected for aggregation
    pub(crate) selected_agg: Option<Vec<PlSmallStr>>,
}

impl<'a> GroupBy<'a> {
    pub fn new(
        df: &'a DataFrame,
        by: Vec<Column>,
        groups: GroupPositions,
        selected_agg: Option<Vec<PlSmallStr>>,
    ) -> Self {
        GroupBy {
            df,
            selected_keys: by,
            groups,
            selected_agg,
        }
    }

    /// Select the column(s) that should be aggregated.
    /// You can select a single column or a slice of columns.
    ///
    /// Note that making a selection with this method is not required. If you
    /// skip it all columns (except for the keys) will be selected for aggregation.
    #[must_use]
    pub fn select<I: IntoIterator<Item = S>, S: Into<PlSmallStr>>(mut self, selection: I) -> Self {
        self.selected_agg = Some(selection.into_iter().map(|s| s.into()).collect());
        self
    }

    /// Get the internal representation of the GroupBy operation.
    /// The Vec returned contains:
    ///     (first_idx, [`Vec<indexes>`])
    ///     Where second value in the tuple is a vector with all matching indexes.
    pub fn get_groups(&self) -> &GroupPositions {
        &self.groups
    }

    /// Get the internal representation of the GroupBy operation.
    /// The Vec returned contains:
    ///     (first_idx, [`Vec<indexes>`])
    ///     Where second value in the tuple is a vector with all matching indexes.
    ///
    /// # Safety
    /// Groups should always be in bounds of the `DataFrame` hold by this [`GroupBy`].
    /// If you mutate it, you must hold that invariant.
    pub unsafe fn get_groups_mut(&mut self) -> &mut GroupPositions {
        &mut self.groups
    }

    pub fn take_groups(self) -> GroupPositions {
        self.groups
    }

    pub fn take_groups_mut(&mut self) -> GroupPositions {
        std::mem::take(&mut self.groups)
    }

    pub fn keys_sliced(&self, slice: Option<(i64, usize)>) -> Vec<Column> {
        #[allow(unused_assignments)]
        // needed to keep the lifetimes valid for this scope
        let mut groups_owned = None;

        let groups = if let Some((offset, len)) = slice {
            groups_owned = Some(self.groups.slice(offset, len));
            groups_owned.as_deref().unwrap()
        } else {
            &self.groups
        };
        POOL.install(|| {
            self.selected_keys
                .par_iter()
                .map(Column::as_materialized_series)
                .map(|s| {
                    match groups {
                        GroupsType::Idx(groups) => {
                            // SAFETY: groups are always in bounds.
                            let mut out = unsafe { s.take_slice_unchecked(groups.first()) };
                            if groups.sorted {
                                out.set_sorted_flag(s.is_sorted_flag());
                            };
                            out
                        },
                        GroupsType::Slice { groups, rolling } => {
                            if *rolling && !groups.is_empty() {
                                // Groups can be sliced.
                                let offset = groups[0][0];
                                let [upper_offset, upper_len] = groups[groups.len() - 1];
                                return s.slice(
                                    offset as i64,
                                    ((upper_offset + upper_len) - offset) as usize,
                                );
                            }

                            let indices = groups
                                .iter()
                                .map(|&[first, _len]| first)
                                .collect_ca(PlSmallStr::EMPTY);
                            // SAFETY: groups are always in bounds.
                            let mut out = unsafe { s.take_unchecked(&indices) };
                            // Sliced groups are always in order of discovery.
                            out.set_sorted_flag(s.is_sorted_flag());
                            out
                        },
                    }
                })
                .map(Column::from)
                .collect()
        })
    }

    pub fn keys(&self) -> Vec<Column> {
        self.keys_sliced(None)
    }

    fn prepare_agg(&self) -> PolarsResult<(Vec<Column>, Vec<Column>)> {
        let keys = self.keys();

        let agg_col = match &self.selected_agg {
            Some(selection) => self.df.select_columns_impl(selection.as_slice()),
            None => {
                let by: Vec<_> = self.selected_keys.iter().map(|s| s.name()).collect();
                let selection = self
                    .df
                    .iter()
                    .map(|s| s.name())
                    .filter(|a| !by.contains(a))
                    .cloned()
                    .collect::<Vec<_>>();

                self.df.select_columns_impl(selection.as_slice())
            },
        }?;

        Ok((keys, agg_col))
    }

    /// Aggregate grouped series and compute the mean per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp", "rain"]).mean()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+-----------+-----------+
    /// | date       | temp_mean | rain_mean |
    /// | ---        | ---       | ---       |
    /// | Date       | f64       | f64       |
    /// +============+===========+===========+
    /// | 2020-08-23 | 9         | 0.1       |
    /// +------------+-----------+-----------+
    /// | 2020-08-22 | 4         | 0.155     |
    /// +------------+-----------+-----------+
    /// | 2020-08-21 | 15        | 0.15      |
    /// +------------+-----------+-----------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn mean(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;

        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Mean);
            let mut agg = unsafe { agg_col.agg_mean(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped series and compute the sum per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).sum()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+----------+
    /// | date       | temp_sum |
    /// | ---        | ---      |
    /// | Date       | i32      |
    /// +============+==========+
    /// | 2020-08-23 | 9        |
    /// +------------+----------+
    /// | 2020-08-22 | 8        |
    /// +------------+----------+
    /// | 2020-08-21 | 30       |
    /// +------------+----------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn sum(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;

        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Sum);
            let mut agg = unsafe { agg_col.agg_sum(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped series and compute the minimal value per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).min()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+----------+
    /// | date       | temp_min |
    /// | ---        | ---      |
    /// | Date       | i32      |
    /// +============+==========+
    /// | 2020-08-23 | 9        |
    /// +------------+----------+
    /// | 2020-08-22 | 1        |
    /// +------------+----------+
    /// | 2020-08-21 | 10       |
    /// +------------+----------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn min(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Min);
            let mut agg = unsafe { agg_col.agg_min(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped series and compute the maximum value per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).max()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+----------+
    /// | date       | temp_max |
    /// | ---        | ---      |
    /// | Date       | i32      |
    /// +============+==========+
    /// | 2020-08-23 | 9        |
    /// +------------+----------+
    /// | 2020-08-22 | 7        |
    /// +------------+----------+
    /// | 2020-08-21 | 20       |
    /// +------------+----------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn max(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Max);
            let mut agg = unsafe { agg_col.agg_max(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped `Series` and find the first value per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).first()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+------------+
    /// | date       | temp_first |
    /// | ---        | ---        |
    /// | Date       | i32        |
    /// +============+============+
    /// | 2020-08-23 | 9          |
    /// +------------+------------+
    /// | 2020-08-22 | 7          |
    /// +------------+------------+
    /// | 2020-08-21 | 20         |
    /// +------------+------------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn first(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::First);
            let mut agg = unsafe { agg_col.agg_first(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped `Series` and return the last value per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).last()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+------------+
    /// | date       | temp_last |
    /// | ---        | ---        |
    /// | Date       | i32        |
    /// +============+============+
    /// | 2020-08-23 | 9          |
    /// +------------+------------+
    /// | 2020-08-22 | 1          |
    /// +------------+------------+
    /// | 2020-08-21 | 10         |
    /// +------------+------------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn last(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Last);
            let mut agg = unsafe { agg_col.agg_last(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped `Series` by counting the number of unique values.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).n_unique()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+---------------+
    /// | date       | temp_n_unique |
    /// | ---        | ---           |
    /// | Date       | u32           |
    /// +============+===============+
    /// | 2020-08-23 | 1             |
    /// +------------+---------------+
    /// | 2020-08-22 | 2             |
    /// +------------+---------------+
    /// | 2020-08-21 | 2             |
    /// +------------+---------------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn n_unique(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::NUnique);
            let mut agg = unsafe { agg_col.agg_n_unique(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped [`Series`] and determine the quantile per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    ///
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).quantile(0.2, QuantileMethod::default())
    /// }
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn quantile(&self, quantile: f64, method: QuantileMethod) -> PolarsResult<DataFrame> {
        polars_ensure!(
            (0.0..=1.0).contains(&quantile),
            ComputeError: "`quantile` should be within 0.0 and 1.0"
        );
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(
                agg_col.name().as_str(),
                GroupByMethod::Quantile(quantile, method),
            );
            let mut agg = unsafe { agg_col.agg_quantile(&self.groups, quantile, method) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped [`Series`] and determine the median per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).median()
    /// }
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn median(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Median);
            let mut agg = unsafe { agg_col.agg_median(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped [`Series`] and determine the variance per group.
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn var(&self, ddof: u8) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Var(ddof));
            let mut agg = unsafe { agg_col.agg_var(&self.groups, ddof) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped [`Series`] and determine the standard deviation per group.
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn std(&self, ddof: u8) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Std(ddof));
            let mut agg = unsafe { agg_col.agg_std(&self.groups, ddof) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    /// Aggregate grouped series and compute the number of values per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.select(["temp"]).count()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+------------+
    /// | date       | temp_count |
    /// | ---        | ---        |
    /// | Date       | u32        |
    /// +============+============+
    /// | 2020-08-23 | 1          |
    /// +------------+------------+
    /// | 2020-08-22 | 2          |
    /// +------------+------------+
    /// | 2020-08-21 | 2          |
    /// +------------+------------+
    /// ```
    pub fn count(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;

        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(
                agg_col.name().as_str(),
                GroupByMethod::Count {
                    include_nulls: true,
                },
            );
            let mut ca = self.groups.group_count();
            ca.rename(new_name);
            cols.push(ca.into_column());
        }
        DataFrame::new(cols)
    }

    /// Get the group_by group indexes.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     df.group_by(["date"])?.groups()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +--------------+------------+
    /// | date         | groups     |
    /// | ---          | ---        |
    /// | Date(days)   | list [u32] |
    /// +==============+============+
    /// | 2020-08-23   | "[3]"      |
    /// +--------------+------------+
    /// | 2020-08-22   | "[2, 4]"   |
    /// +--------------+------------+
    /// | 2020-08-21   | "[0, 1]"   |
    /// +--------------+------------+
    /// ```
    pub fn groups(&self) -> PolarsResult<DataFrame> {
        let mut cols = self.keys();
        let mut column = self.groups.as_list_chunked();
        let new_name = fmt_group_by_column("", GroupByMethod::Groups);
        column.rename(new_name);
        cols.push(column.into_column());
        DataFrame::new(cols)
    }

    /// Aggregate the groups of the group_by operation into lists.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars_core::prelude::*;
    /// fn example(df: DataFrame) -> PolarsResult<DataFrame> {
    ///     // GroupBy and aggregate to Lists
    ///     df.group_by(["date"])?.select(["temp"]).agg_list()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+------------------------+
    /// | date       | temp_agg_list          |
    /// | ---        | ---                    |
    /// | Date       | list [i32]             |
    /// +============+========================+
    /// | 2020-08-23 | "[Some(9)]"            |
    /// +------------+------------------------+
    /// | 2020-08-22 | "[Some(7), Some(1)]"   |
    /// +------------+------------------------+
    /// | 2020-08-21 | "[Some(20), Some(10)]" |
    /// +------------+------------------------+
    /// ```
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn agg_list(&self) -> PolarsResult<DataFrame> {
        let (mut cols, agg_cols) = self.prepare_agg()?;
        for agg_col in agg_cols {
            let new_name = fmt_group_by_column(agg_col.name().as_str(), GroupByMethod::Implode);
            let mut agg = unsafe { agg_col.agg_list(&self.groups) };
            agg.rename(new_name);
            cols.push(agg);
        }
        DataFrame::new(cols)
    }

    fn prepare_apply(&self) -> PolarsResult<DataFrame> {
        polars_ensure!(self.df.height() > 0, ComputeError: "cannot group_by + apply on empty 'DataFrame'");
        if let Some(agg) = &self.selected_agg {
            if agg.is_empty() {
                Ok(self.df.clone())
            } else {
                let mut new_cols = Vec::with_capacity(self.selected_keys.len() + agg.len());
                new_cols.extend_from_slice(&self.selected_keys);
                let cols = self.df.select_columns_impl(agg.as_slice())?;
                new_cols.extend(cols);
                Ok(unsafe { DataFrame::new_no_checks(self.df.height(), new_cols) })
            }
        } else {
            Ok(self.df.clone())
        }
    }

    /// Apply a closure over the groups as a new [`DataFrame`] in parallel.
    #[deprecated(since = "0.24.1", note = "use polars.lazy aggregations")]
    pub fn par_apply<F>(&self, f: F) -> PolarsResult<DataFrame>
    where
        F: Fn(DataFrame) -> PolarsResult<DataFrame> + Send + Sync,
    {
        let df = self.prepare_apply()?;
        let dfs = self
            .get_groups()
            .par_iter()
            .map(|g| {
                // SAFETY:
                // groups are in bounds
                let sub_df = unsafe { take_df(&df, g) };
                f(sub_df)
            })
            .collect::<PolarsResult<Vec<_>>>()?;

        let mut df = accumulate_dataframes_vertical(dfs)?;
        df.as_single_chunk_par();
        Ok(df)
    }

    /// Apply a closure over the groups as a new [`DataFrame`].
    pub fn apply<F>(&self, mut f: F) -> PolarsResult<DataFrame>
    where
        F: FnMut(DataFrame) -> PolarsResult<DataFrame> + Send + Sync,
    {
        let df = self.prepare_apply()?;
        let dfs = self
            .get_groups()
            .iter()
            .map(|g| {
                // SAFETY:
                // groups are in bounds
                let sub_df = unsafe { take_df(&df, g) };
                f(sub_df)
            })
            .collect::<PolarsResult<Vec<_>>>()?;

        let mut df = accumulate_dataframes_vertical(dfs)?;
        df.as_single_chunk_par();
        Ok(df)
    }

    pub fn sliced(mut self, slice: Option<(i64, usize)>) -> Self {
        match slice {
            None => self,
            Some((offset, length)) => {
                self.groups = self.groups.slice(offset, length);
                self.selected_keys = self.keys_sliced(slice);
                self
            },
        }
    }
}

unsafe fn take_df(df: &DataFrame, g: GroupsIndicator) -> DataFrame {
    match g {
        GroupsIndicator::Idx(idx) => df.take_slice_unchecked(idx.1),
        GroupsIndicator::Slice([first, len]) => df.slice(first as i64, len as usize),
    }
}

#[derive(Copy, Clone, Debug)]
pub enum GroupByMethod {
    Min,
    NanMin,
    Max,
    NanMax,
    Median,
    Mean,
    First,
    Last,
    Sum,
    Groups,
    NUnique,
    Quantile(f64, QuantileMethod),
    Count { include_nulls: bool },
    Implode,
    Std(u8),
    Var(u8),
}

impl Display for GroupByMethod {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        use GroupByMethod::*;
        let s = match self {
            Min => "min",
            NanMin => "nan_min",
            Max => "max",
            NanMax => "nan_max",
            Median => "median",
            Mean => "mean",
            First => "first",
            Last => "last",
            Sum => "sum",
            Groups => "groups",
            NUnique => "n_unique",
            Quantile(_, _) => "quantile",
            Count { .. } => "count",
            Implode => "list",
            Std(_) => "std",
            Var(_) => "var",
        };
        write!(f, "{s}")
    }
}

// Formatting functions used in eager and lazy code for renaming grouped columns
pub fn fmt_group_by_column(name: &str, method: GroupByMethod) -> PlSmallStr {
    use GroupByMethod::*;
    match method {
        Min => format_pl_smallstr!("{name}_min"),
        Max => format_pl_smallstr!("{name}_max"),
        NanMin => format_pl_smallstr!("{name}_nan_min"),
        NanMax => format_pl_smallstr!("{name}_nan_max"),
        Median => format_pl_smallstr!("{name}_median"),
        Mean => format_pl_smallstr!("{name}_mean"),
        First => format_pl_smallstr!("{name}_first"),
        Last => format_pl_smallstr!("{name}_last"),
        Sum => format_pl_smallstr!("{name}_sum"),
        Groups => PlSmallStr::from_static("groups"),
        NUnique => format_pl_smallstr!("{name}_n_unique"),
        Count { .. } => format_pl_smallstr!("{name}_count"),
        Implode => format_pl_smallstr!("{name}_agg_list"),
        Quantile(quantile, _interpol) => format_pl_smallstr!("{name}_quantile_{quantile:.2}"),
        Std(_) => format_pl_smallstr!("{name}_agg_std"),
        Var(_) => format_pl_smallstr!("{name}_agg_var"),
    }
}

#[cfg(test)]
mod test {
    use num_traits::FloatConst;

    use crate::prelude::*;

    #[test]
    #[cfg(feature = "dtype-date")]
    #[cfg_attr(miri, ignore)]
    fn test_group_by() -> PolarsResult<()> {
        let s0 = Column::new(
            PlSmallStr::from_static("date"),
            &[
                "2020-08-21",
                "2020-08-21",
                "2020-08-22",
                "2020-08-23",
                "2020-08-22",
            ],
        );
        let s1 = Column::new(PlSmallStr::from_static("temp"), [20, 10, 7, 9, 1]);
        let s2 = Column::new(PlSmallStr::from_static("rain"), [0.2, 0.1, 0.3, 0.1, 0.01]);
        let df = DataFrame::new(vec![s0, s1, s2]).unwrap();

        let out = df.group_by_stable(["date"])?.select(["temp"]).count()?;
        assert_eq!(
            out.column("temp_count")?,
            &Column::new(PlSmallStr::from_static("temp_count"), [2 as IdxSize, 2, 1])
        );

        // Use of deprecated mean() for testing purposes
        #[allow(deprecated)]
        // Select multiple
        let out = df
            .group_by_stable(["date"])?
            .select(["temp", "rain"])
            .mean()?;
        assert_eq!(
            out.column("temp_mean")?,
            &Column::new(PlSmallStr::from_static("temp_mean"), [15.0f64, 4.0, 9.0])
        );

        // Use of deprecated `mean()` for testing purposes
        #[allow(deprecated)]
        // Group by multiple
        let out = df
            .group_by_stable(["date", "temp"])?
            .select(["rain"])
            .mean()?;
        assert!(out.column("rain_mean").is_ok());

        // Use of deprecated `sum()` for testing purposes
        #[allow(deprecated)]
        let out = df.group_by_stable(["date"])?.select(["temp"]).sum()?;
        assert_eq!(
            out.column("temp_sum")?,
            &Column::new(PlSmallStr::from_static("temp_sum"), [30, 8, 9])
        );

        // Use of deprecated `n_unique()` for testing purposes
        #[allow(deprecated)]
        // implicit select all and only aggregate on methods that support that aggregation
        let gb = df.group_by(["date"]).unwrap().n_unique().unwrap();
        // check the group by column is filtered out.
        assert_eq!(gb.width(), 3);
        Ok(())
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_static_group_by_by_12_columns() {
        // Build GroupBy DataFrame.
        let s0 = Column::new("G1".into(), ["A", "A", "B", "B", "C"].as_ref());
        let s1 = Column::new("N".into(), [1, 2, 2, 4, 2].as_ref());
        let s2 = Column::new("G2".into(), ["k", "l", "m", "m", "l"].as_ref());
        let s3 = Column::new("G3".into(), ["a", "b", "c", "c", "d"].as_ref());
        let s4 = Column::new("G4".into(), ["1", "2", "3", "3", "4"].as_ref());
        let s5 = Column::new("G5".into(), ["X", "Y", "Z", "Z", "W"].as_ref());
        let s6 = Column::new("G6".into(), [false, true, true, true, false].as_ref());
        let s7 = Column::new("G7".into(), ["r", "x", "q", "q", "o"].as_ref());
        let s8 = Column::new("G8".into(), ["R", "X", "Q", "Q", "O"].as_ref());
        let s9 = Column::new("G9".into(), [1, 2, 3, 3, 4].as_ref());
        let s10 = Column::new("G10".into(), [".", "!", "?", "?", "/"].as_ref());
        let s11 = Column::new("G11".into(), ["(", ")", "@", "@", "$"].as_ref());
        let s12 = Column::new("G12".into(), ["-", "_", ";", ";", ","].as_ref());

        let df =
            DataFrame::new(vec![s0, s1, s2, s3, s4, s5, s6, s7, s8, s9, s10, s11, s12]).unwrap();

        // Use of deprecated `sum()` for testing purposes
        #[allow(deprecated)]
        let adf = df
            .group_by([
                "G1", "G2", "G3", "G4", "G5", "G6", "G7", "G8", "G9", "G10", "G11", "G12",
            ])
            .unwrap()
            .select(["N"])
            .sum()
            .unwrap();

        assert_eq!(
            Vec::from(&adf.column("N_sum").unwrap().i32().unwrap().sort(false)),
            &[Some(1), Some(2), Some(2), Some(6)]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_dynamic_group_by_by_13_columns() {
        // The content for every group_by series.
        let series_content = ["A", "A", "B", "B", "C"];

        // The name of every group_by series.
        let series_names = [
            "G1", "G2", "G3", "G4", "G5", "G6", "G7", "G8", "G9", "G10", "G11", "G12", "G13",
        ];

        // Vector to contain every series.
        let mut columns = Vec::with_capacity(14);

        // Create a series for every group name.
        for series_name in series_names {
            let group_columns = Column::new(series_name.into(), series_content.as_ref());
            columns.push(group_columns);
        }

        // Create a series for the aggregation column.
        let agg_series = Column::new("N".into(), [1, 2, 3, 3, 4].as_ref());
        columns.push(agg_series);

        // Create the dataframe with the computed series.
        let df = DataFrame::new(columns).unwrap();

        // Use of deprecated `sum()` for testing purposes
        #[allow(deprecated)]
        // Compute the aggregated DataFrame by the 13 columns defined in `series_names`.
        let adf = df
            .group_by(series_names)
            .unwrap()
            .select(["N"])
            .sum()
            .unwrap();

        // Check that the results of the group-by are correct. The content of every column
        // is equal, then, the grouped columns shall be equal and in the same order.
        for series_name in &series_names {
            assert_eq!(
                Vec::from(&adf.column(series_name).unwrap().str().unwrap().sort(false)),
                &[Some("A"), Some("B"), Some("C")]
            );
        }

        // Check the aggregated column is the expected one.
        assert_eq!(
            Vec::from(&adf.column("N_sum").unwrap().i32().unwrap().sort(false)),
            &[Some(3), Some(4), Some(6)]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_group_by_floats() {
        let df = df! {"flt" => [1., 1., 2., 2., 3.],
                    "val" => [1, 1, 1, 1, 1]
        }
        .unwrap();
        // Use of deprecated `sum()` for testing purposes
        #[allow(deprecated)]
        let res = df.group_by(["flt"]).unwrap().sum().unwrap();
        let res = res.sort(["flt"], SortMultipleOptions::default()).unwrap();
        assert_eq!(
            Vec::from(res.column("val_sum").unwrap().i32().unwrap()),
            &[Some(2), Some(2), Some(1)]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    #[cfg(feature = "dtype-categorical")]
    fn test_group_by_categorical() {
        let mut df = df! {"foo" => ["a", "a", "b", "b", "c"],
                    "ham" => ["a", "a", "b", "b", "c"],
                    "bar" => [1, 1, 1, 1, 1]
        }
        .unwrap();

        df.apply("foo", |s| {
            s.cast(&DataType::from_categories(Categories::global()))
                .unwrap()
        })
        .unwrap();

        // Use of deprecated `sum()` for testing purposes
        #[allow(deprecated)]
        // check multiple keys and categorical
        let res = df
            .group_by_stable(["foo", "ham"])
            .unwrap()
            .select(["bar"])
            .sum()
            .unwrap();

        assert_eq!(
            Vec::from(
                res.column("bar_sum")
                    .unwrap()
                    .as_materialized_series()
                    .i32()
                    .unwrap()
            ),
            &[Some(2), Some(2), Some(1)]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_group_by_null_handling() -> PolarsResult<()> {
        let df = df!(
            "a" => ["a", "a", "a", "b", "b"],
            "b" => [Some(1), Some(2), None, None, Some(1)]
        )?;
        // Use of deprecated `mean()` for testing purposes
        #[allow(deprecated)]
        let out = df.group_by_stable(["a"])?.mean()?;

        assert_eq!(
            Vec::from(out.column("b_mean")?.as_materialized_series().f64()?),
            &[Some(1.5), Some(1.0)]
        );
        Ok(())
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_group_by_var() -> PolarsResult<()> {
        // check variance and proper coercion to f64
        let df = df![
            "g" => ["foo", "foo", "bar"],
            "flt" => [1.0, 2.0, 3.0],
            "int" => [1, 2, 3]
        ]?;

        // Use of deprecated `sum()` for testing purposes
        #[allow(deprecated)]
        let out = df.group_by_stable(["g"])?.select(["int"]).var(1)?;

        assert_eq!(out.column("int_agg_var")?.f64()?.get(0), Some(0.5));
        // Use of deprecated `std()` for testing purposes
        #[allow(deprecated)]
        let out = df.group_by_stable(["g"])?.select(["int"]).std(1)?;
        let val = out.column("int_agg_std")?.f64()?.get(0).unwrap();
        let expected = f64::FRAC_1_SQRT_2();
        assert!((val - expected).abs() < 0.000001);
        Ok(())
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    #[cfg(feature = "dtype-categorical")]
    fn test_group_by_null_group() -> PolarsResult<()> {
        // check if null is own group
        let mut df = df![
            "g" => [Some("foo"), Some("foo"), Some("bar"), None, None],
            "flt" => [1.0, 2.0, 3.0, 1.0, 1.0],
            "int" => [1, 2, 3, 1, 1]
        ]?;

        df.try_apply("g", |s| {
            s.cast(&DataType::from_categories(Categories::global()))
        })?;

        // Use of deprecated `sum()` for testing purposes
        #[allow(deprecated)]
        let _ = df.group_by(["g"])?.sum()?;
        Ok(())
    }
}

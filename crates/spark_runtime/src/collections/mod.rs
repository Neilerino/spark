//! Collection runtime implementations.

mod hash;
mod list;
mod map;
mod set;
mod storage;

const MIN_CAP: usize = 4;

pub use list::{
    SparkList, __spark_list_contains, __spark_list_cow, __spark_list_from_range, __spark_list_get,
    __spark_list_len, __spark_list_new, __spark_list_push, __spark_list_rc_dec,
    __spark_list_remove, __spark_list_reverse, __spark_list_sort, __spark_list_take, SORT_TAG_BOOL,
    SORT_TAG_FLOAT, SORT_TAG_INT, SORT_TAG_STRING,
};
pub use map::{
    SparkMap, __spark_map_contains, __spark_map_cow, __spark_map_get, __spark_map_insert,
    __spark_map_len, __spark_map_new, __spark_map_rc_dec, __spark_map_remove,
};
pub use set::{
    SparkSet, __spark_set_contains, __spark_set_cow, __spark_set_insert, __spark_set_len,
    __spark_set_new, __spark_set_rc_dec, __spark_set_remove,
};

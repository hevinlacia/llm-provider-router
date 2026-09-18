//! 配置校验与文本工具（纯函数）。

use std::collections::{HashMap, HashSet};

pub(crate) fn sorted_join(mut values: Vec<String>) -> String {
    values.sort();
    values.dedup();
    values.join(", ")
}

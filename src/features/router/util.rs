//! 配置校验与文本工具（纯函数）。


pub(crate) fn sorted_join(mut values: Vec<String>) -> String {
    values.sort();
    values.dedup();
    values.join(", ")
}

//! 平台数据目录解析。
//!
//! 运行时目录与凭据目录都挂在「用户数据根目录」下，而各平台惯例不同：POSIX 认
//! `$XDG_DATA_HOME` / `$HOME`，Windows 上 `$HOME` 通常**根本不存在**（只有
//! `%USERPROFILE%` / `%APPDATA%`）。只判前两者会让 `dossier-runtime secret` 与默认
//! 运行时目录在 Windows 上直接不可用——CI 的 windows runner 就是这种环境
//! （实测 `HOME`、`XDG_DATA_HOME` 均为空，`APPDATA` 有值）。
//!
//! 这里只回答「根目录在哪」，具体子目录（`dossier/runtime`、`dossier/secrets`）
//! 由调用方拼接。

use std::ffi::OsString;
use std::path::PathBuf;

/// 用户数据根目录，按平台惯例取第一个可用来源：
///
/// 1. `$XDG_DATA_HOME`（POSIX，存在即用它本身）；
/// 2. `$HOME/.local/share`（POSIX 回退）；
/// 3. `%APPDATA%`（Windows 的漫游应用数据目录）。
///
/// 三处都没有时返回 `None`，由调用方决定报什么错（CLI 提示 `--root`，凭据存储报 `NoHome`）。
pub fn default_data_root() -> Option<PathBuf> {
    resolve_data_root(|key| std::env::var_os(key))
}

/// [`default_data_root`] 的纯函数形式：把「怎么读环境变量」注入进来，
/// 这样各平台的分支都能在任意平台上测到（不必改进程环境，避免并行测试互相干扰）。
fn resolve_data_root(lookup: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let path = |key: &str| {
        lookup(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(xdg) = path("XDG_DATA_HOME") {
        return Some(xdg);
    }
    if let Some(home) = path("HOME") {
        return Some(home.join(".local").join("share"));
    }
    path("APPDATA")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个只有给定键值的「环境」。
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(*value)))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn xdg_wins_and_is_used_as_is() {
        let root = resolve_data_root(env(&[
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/u"),
            ("APPDATA", "C:\\Users\\u\\AppData\\Roaming"),
        ]))
        .unwrap();
        assert_eq!(root, PathBuf::from("/xdg"));
    }

    #[test]
    fn home_falls_back_to_local_share() {
        let root = resolve_data_root(env(&[
            ("HOME", "/home/u"),
            ("APPDATA", "C:\\Users\\u\\AppData\\Roaming"),
        ]))
        .unwrap();
        assert_eq!(root, PathBuf::from("/home/u/.local/share"));
    }

    /// Windows 的真实情况：没有 XDG / HOME，只有 `%APPDATA%`。
    #[test]
    fn appdata_covers_windows_where_home_does_not_exist() {
        let root =
            resolve_data_root(env(&[("APPDATA", "C:\\Users\\u\\AppData\\Roaming")])).unwrap();
        assert_eq!(root, PathBuf::from("C:\\Users\\u\\AppData\\Roaming"));
    }

    #[test]
    fn empty_values_count_as_unset_and_no_source_is_none() {
        assert!(resolve_data_root(env(
            &[("XDG_DATA_HOME", ""), ("HOME", ""), ("APPDATA", ""),]
        ))
        .is_none());
        assert!(resolve_data_root(env(&[])).is_none());
    }
}

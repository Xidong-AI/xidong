//! 状态根目录解析。
//!
//! 所有函数都是纯函数：环境查询由调用方注入，测试因此无需改动真实进程环境。
//! 按 `docs/plan.md` 的 D10 拆分配置根与数据根，`GLB_CFG` 指向配置根。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 读取环境变量并忽略空值（空字符串按未设置处理，与 shell 语义一致）。
fn env_non_empty(env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    env(key).filter(|value| !value.is_empty())
}

/// 配置根：`$YAA_CONFIG_HOME` 优先，其次 `${XDG_CONFIG_HOME:-$HOME/.config}/yaa`。
pub fn config_root(env: &impl Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    if let Some(value) = env_non_empty(env, "YAA_CONFIG_HOME") {
        return PathBuf::from(value);
    }
    let base = env_non_empty(env, "XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    base.join("yaa")
}

/// 数据根：`$YAA_DATA_HOME` 优先，其次 `${XDG_DATA_HOME:-$HOME/.local/share}/yaa`。
pub fn data_root(env: &impl Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    if let Some(value) = env_non_empty(env, "YAA_DATA_HOME") {
        return PathBuf::from(value);
    }
    let base = env_non_empty(env, "XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local").join("share"));
    base.join("yaa")
}

/// `config.toml` 路径：`<配置根>/config.toml`。
pub fn config_file(env: &impl Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    config_root(env, home).join("config.toml")
}

/// 钩子进程的环境变量映射。
///
/// `GLB_CFG` 兼容 `docs/design.md` 的旧写法，与 `YAA_CONFIG_HOME` 同值。
pub fn hook_env(env: &impl Fn(&str) -> Option<String>, home: &Path) -> BTreeMap<String, String> {
    let config = config_root(env, home);
    let data = data_root(env, home);
    let mut map = BTreeMap::new();
    map.insert("GLB_CFG".to_string(), config.to_string_lossy().into_owned());
    map.insert(
        "YAA_CONFIG_HOME".to_string(),
        config.to_string_lossy().into_owned(),
    );
    map.insert(
        "YAA_DATA_HOME".to_string(),
        data.to_string_lossy().into_owned(),
    );
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    #[test]
    fn defaults_without_env() {
        let env = lookup(&[]);
        assert_eq!(config_root(&env, &home()), home().join(".config/yaa"));
        assert_eq!(data_root(&env, &home()), home().join(".local/share/yaa"));
    }

    #[test]
    fn yaa_overrides_win_over_xdg() {
        let env = lookup(&[
            ("YAA_CONFIG_HOME", "/custom/config"),
            ("XDG_CONFIG_HOME", "/xdg/config"),
            ("YAA_DATA_HOME", "/custom/data"),
            ("XDG_DATA_HOME", "/xdg/data"),
        ]);
        assert_eq!(config_root(&env, &home()), PathBuf::from("/custom/config"));
        assert_eq!(data_root(&env, &home()), PathBuf::from("/custom/data"));
    }

    #[test]
    fn xdg_fallbacks_are_used() {
        let env = lookup(&[
            ("XDG_CONFIG_HOME", "/xdg/config"),
            ("XDG_DATA_HOME", "/xdg/data"),
        ]);
        assert_eq!(config_root(&env, &home()), PathBuf::from("/xdg/config/yaa"));
        assert_eq!(data_root(&env, &home()), PathBuf::from("/xdg/data/yaa"));
    }

    #[test]
    fn empty_values_count_as_unset() {
        let env = lookup(&[("YAA_CONFIG_HOME", ""), ("YAA_DATA_HOME", "")]);
        assert_eq!(config_root(&env, &home()), home().join(".config/yaa"));
        assert_eq!(data_root(&env, &home()), home().join(".local/share/yaa"));
    }

    #[test]
    fn config_file_lives_in_config_root() {
        let env = lookup(&[]);
        assert_eq!(
            config_file(&env, &home()),
            home().join(".config/yaa/config.toml")
        );
    }

    #[test]
    fn hook_env_exposes_both_roots() {
        let env = lookup(&[]);
        let map = hook_env(&env, &home());
        let config = home().join(".config/yaa");
        let data = home().join(".local/share/yaa");
        assert_eq!(map.get("GLB_CFG").unwrap(), &config.to_string_lossy());
        assert_eq!(
            map.get("YAA_CONFIG_HOME").unwrap(),
            &config.to_string_lossy()
        );
        assert_eq!(map.get("YAA_DATA_HOME").unwrap(), &data.to_string_lossy());
    }
}

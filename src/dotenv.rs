//! `.env` 的最小解析与三级查找。`ai`（LLM 端点）与 `sync`（Worker 地址与密钥）共用。
//!
//! 不引 dotenv crate —— 这个文件只有我自己写，不需要支持多行值与变量展开。

use std::collections::HashMap;

use crate::store::Store;

/// 最小 `.env` 解析：`KEY=VALUE` 逐行，跳过空行与 `#` 注释，去掉成对的引号。
pub fn parse(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        out.insert(k.trim().to_string(), v.to_string());
    }
    out
}

/// 三级合并，后者压前者：用户数据目录的 `.env` → 当前目录的 `.env` → 进程环境。
///
/// 空值不参与覆盖 —— `.env.example` 里那些 `KEY=` 的空行不该把上一层的真值抹掉。
/// 纯函数，测试直接喂三层来源。
pub fn merge(
    keys: &[&str],
    env: impl Fn(&str) -> Option<String>,
    cwd_text: &str,
    data_text: &str,
) -> HashMap<String, String> {
    let mut out = parse(data_text);
    for (k, v) in parse(cwd_text) {
        if !v.trim().is_empty() {
            out.insert(k, v);
        }
    }
    for k in keys {
        if let Some(v) = env(k)
            && !v.trim().is_empty()
        {
            out.insert((*k).to_string(), v);
        }
    }
    out.retain(|_, v| !v.trim().is_empty());
    out
}

/// 实际去三个地方找。数据目录那份是「装在哪都能跑」的那条路 ——
/// 从桌面双击起 souba 时当前目录不是仓库，仓库里的 `.env` 根本读不到。
pub fn load(keys: &[&str]) -> HashMap<String, String> {
    let data = Store::default_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(".env")))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let cwd = std::fs::read_to_string(".env").unwrap_or_default();
    merge(keys, |k| std::env::var(k).ok(), &cwd, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn 解析跳过注释与空行并去引号() {
        let m = parse(
            "# 注释\n\n\
             LLM_API_KEY=sk-123\n\
             export LLM_MODEL=\"qwen3.7-flash\"\n\
             LLM_BASE_URL = 'https://x.test/v1' \n\
             没有等号的一行\n\
             EMPTY=\n",
        );
        assert_eq!(m.get("LLM_API_KEY").unwrap(), "sk-123");
        assert_eq!(m.get("LLM_MODEL").unwrap(), "qwen3.7-flash");
        assert_eq!(m.get("LLM_BASE_URL").unwrap(), "https://x.test/v1");
        assert_eq!(m.get("EMPTY").unwrap(), "");
        assert!(!m.contains_key("# 注释"));
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn 值里带等号不被截断() {
        let m = parse("LLM_API_KEY=a=b=c\n");
        assert_eq!(m.get("LLM_API_KEY").unwrap(), "a=b=c");
    }

    #[test]
    fn 数据目录的值在没有其他来源时生效() {
        let m = merge(&["SOUBA_SYNC_KEY"], no_env, "", "SOUBA_SYNC_KEY=from-data\n");
        assert_eq!(m.get("SOUBA_SYNC_KEY").unwrap(), "from-data");
    }

    #[test]
    fn 当前目录压过数据目录() {
        let m = merge(
            &["SOUBA_SYNC_KEY"],
            no_env,
            "SOUBA_SYNC_KEY=from-cwd\n",
            "SOUBA_SYNC_KEY=from-data\n",
        );
        assert_eq!(m.get("SOUBA_SYNC_KEY").unwrap(), "from-cwd");
    }

    #[test]
    fn 进程环境压过两个文件() {
        let m = merge(
            &["SOUBA_SYNC_KEY"],
            |k| (k == "SOUBA_SYNC_KEY").then(|| "from-env".to_string()),
            "SOUBA_SYNC_KEY=from-cwd\n",
            "SOUBA_SYNC_KEY=from-data\n",
        );
        assert_eq!(m.get("SOUBA_SYNC_KEY").unwrap(), "from-env");
    }

    #[test]
    fn 空值不覆盖下层的真值() {
        let m = merge(
            &["SOUBA_SYNC_KEY"],
            |_| Some("   ".to_string()),
            "SOUBA_SYNC_KEY=\n",
            "SOUBA_SYNC_KEY=from-data\n",
        );
        assert_eq!(m.get("SOUBA_SYNC_KEY").unwrap(), "from-data");
    }

    #[test]
    fn 三层都没有就查不到() {
        let m = merge(&["SOUBA_SYNC_KEY"], no_env, "OTHER=1\n", "");
        assert!(!m.contains_key("SOUBA_SYNC_KEY"));
    }
}

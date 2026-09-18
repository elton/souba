//! 策略与扫描参数：键定义、默认值、校验、从库装载，以及 set/get/settings 三个子命令。

use crate::store::Store;

/// Vegas 通道参数。默认值见 spec §策略与排序。
#[derive(Debug, Clone, PartialEq)]
pub struct VegasParams {
    pub filter: usize,
    pub fast: (usize, usize),
    pub slow: (usize, usize),
    pub min_bars: usize,
    pub slope_window: usize,
    pub fresh_window: usize,
}

/// 板块扫描参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ScanParams {
    pub sectors: usize,
    pub k: usize,
    pub top: usize,
    pub heat_days: usize,
    pub w_turnover: f64,
    pub w_change: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub vegas: VegasParams,
    pub scan: ScanParams,
    /// 空表示用 `.env` 里的 `LLM_MODEL`
    pub ai_model: String,
}

impl Default for VegasParams {
    fn default() -> Self {
        Self {
            filter: 12,
            fast: (144, 169),
            slow: (576, 676),
            min_bars: 1330,
            slope_window: 5,
            fresh_window: 3,
        }
    }
}

impl Default for ScanParams {
    fn default() -> Self {
        Self {
            sectors: 8,
            k: 20,
            top: 5,
            heat_days: 5,
            w_turnover: 1.0,
            w_change: 1.0,
        }
    }
}

/// `souba settings` 的列出顺序。每个键都必须能被 `apply` 与 `show` 处理（有测试保证）。
pub const KEYS: &[&str] = &[
    "vegas.filter",
    "vegas.fast",
    "vegas.slow",
    "vegas.min_bars",
    "vegas.slope_window",
    "vegas.fresh_window",
    "scan.sectors",
    "scan.k",
    "scan.top",
    "scan.heat_days",
    "scan.w_turnover",
    "scan.w_change",
    "ai.model",
];

fn as_usize(v: &str) -> Result<usize, String> {
    v.trim()
        .parse()
        .map_err(|_| format!("需要一个正整数，得到 {:?}", v.trim()))
}

fn as_pair(v: &str) -> Result<(usize, usize), String> {
    let parts: Vec<&str> = v.split(',').collect();
    if parts.len() != 2 {
        return Err(format!(
            "隧道要写成用逗号隔开的两个周期，例如 144,169，得到 {:?}",
            v.trim()
        ));
    }
    Ok((as_usize(parts[0])?, as_usize(parts[1])?))
}

fn as_f64(v: &str) -> Result<f64, String> {
    let n: f64 = v
        .trim()
        .parse()
        .map_err(|_| format!("需要一个数字，得到 {:?}", v.trim()))?;
    if !n.is_finite() {
        return Err(format!("需要一个有限数字，得到 {:?}", v.trim()));
    }
    Ok(n)
}

/// 权重回显成 `1.0` 而不是 `1`，读起来才像个浮点参数
fn show_f64(n: f64) -> String {
    format!("{n:?}")
}

impl Settings {
    /// 从库装载，库里没有的键取默认值。
    ///
    /// 单条坏记录只跳过不整体失败 —— 手改过库不该让 TUI 打不开，
    /// 跟 `Store::watchlist` 对无法解析的代码是同一个态度。
    pub fn load(store: &Store) -> anyhow::Result<Self> {
        let mut s = Self::default();
        for (k, v) in store.all_settings()? {
            if let Err(e) = s.apply(&k, &v) {
                eprintln!("[souba] 忽略库里的设置 {k}={v:?}：{e}");
            }
        }
        Ok(s)
    }

    /// 只做类型解析，跨键的约束在 `validate` 里 —— 改一个键要拿整份参数去看合不合法
    pub fn apply(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "vegas.filter" => self.vegas.filter = as_usize(value)?,
            "vegas.fast" => self.vegas.fast = as_pair(value)?,
            "vegas.slow" => self.vegas.slow = as_pair(value)?,
            "vegas.min_bars" => self.vegas.min_bars = as_usize(value)?,
            "vegas.slope_window" => self.vegas.slope_window = as_usize(value)?,
            "vegas.fresh_window" => self.vegas.fresh_window = as_usize(value)?,
            "scan.sectors" => self.scan.sectors = as_usize(value)?,
            "scan.k" => self.scan.k = as_usize(value)?,
            "scan.top" => self.scan.top = as_usize(value)?,
            "scan.heat_days" => self.scan.heat_days = as_usize(value)?,
            "scan.w_turnover" => self.scan.w_turnover = as_f64(value)?,
            "scan.w_change" => self.scan.w_change = as_f64(value)?,
            "ai.model" => self.ai_model = value.trim().to_string(),
            _ => return Err(format!("未知的参数键 {key:?}，可用键见 souba settings")),
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        let v = &self.vegas;
        // heat_days 可以是 0（spec：0 日时退化为当日涨幅），其余计数都必须为正
        for (name, n) in [
            ("vegas.filter", v.filter),
            ("vegas.slope_window", v.slope_window),
            ("vegas.fresh_window", v.fresh_window),
            ("scan.sectors", self.scan.sectors),
            ("scan.k", self.scan.k),
            ("scan.top", self.scan.top),
        ] {
            if n == 0 {
                return Err(format!("{name} 不能为 0"));
            }
        }
        for (name, p) in [("vegas.fast", v.fast), ("vegas.slow", v.slow)] {
            if p.0 == 0 || p.1 == 0 {
                return Err(format!("{name} 的周期不能为 0（得到 {},{}）", p.0, p.1));
            }
        }
        if v.fast.0.min(v.fast.1) >= v.slow.0.min(v.slow.1) {
            return Err(format!(
                "快隧道 {},{} 必须比慢隧道 {},{} 快（按各自较短的那条比）",
                v.fast.0, v.fast.1, v.slow.0, v.slow.1
            ));
        }
        let longest = v.slow.0.max(v.slow.1);
        if v.min_bars < longest {
            return Err(format!(
                "vegas.min_bars {} 小于慢隧道最长周期 {longest}，这么少的根数算不出 EMA",
                v.min_bars
            ));
        }
        for (name, w) in [
            ("scan.w_turnover", self.scan.w_turnover),
            ("scan.w_change", self.scan.w_change),
        ] {
            if w < 0.0 {
                return Err(format!("{name} 是权重，不能为负（得到 {}）", show_f64(w)));
            }
        }
        Ok(())
    }

    /// 键的当前值文本。未知键返回 None。写回库和打印用的是同一份文本，
    /// 所以库里存的永远是规范化后的写法。
    pub fn show(&self, key: &str) -> Option<String> {
        let v = &self.vegas;
        let s = &self.scan;
        Some(match key {
            "vegas.filter" => v.filter.to_string(),
            "vegas.fast" => format!("{},{}", v.fast.0, v.fast.1),
            "vegas.slow" => format!("{},{}", v.slow.0, v.slow.1),
            "vegas.min_bars" => v.min_bars.to_string(),
            "vegas.slope_window" => v.slope_window.to_string(),
            "vegas.fresh_window" => v.fresh_window.to_string(),
            "scan.sectors" => s.sectors.to_string(),
            "scan.k" => s.k.to_string(),
            "scan.top" => s.top.to_string(),
            "scan.heat_days" => s.heat_days.to_string(),
            "scan.w_turnover" => show_f64(s.w_turnover),
            "scan.w_change" => show_f64(s.w_change),
            "ai.model" => self.ai_model.clone(),
            _ => return None,
        })
    }
}

/// 按显示宽度补空格 —— 列里会出现中文表头，按字符数补会错位
fn pad(s: &str, cells: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(cells.saturating_sub(w)))
}

fn shown(s: &Settings, key: &str) -> String {
    match s.show(key) {
        Some(v) if v.is_empty() => "(空)".to_string(),
        Some(v) => v,
        None => unreachable!("KEYS 里的键都有展示值"),
    }
}

/// `set` / `get` / `settings` 三个子命令。出错返回 Err，main 会以非 0 退出码打印它。
pub fn run(cmd: &str, store: &Store, args: &[String]) -> anyhow::Result<()> {
    let mut cur = Settings::load(store)?;
    let def = Settings::default();
    match cmd {
        "set" => {
            let [key, value] = args else {
                anyhow::bail!("用法：souba set <键> <值>，例如 souba set vegas.fast 144,169");
            };
            cur.apply(key, value).map_err(|e| anyhow::anyhow!("{e}"))?;
            cur.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
            let normalized = cur.show(key).expect("apply 通过说明键存在");
            store.set_setting(key, &normalized)?;
            println!("{key} = {}（默认 {}）", shown(&cur, key), shown(&def, key));
        }
        "get" => {
            let [key] = args else {
                anyhow::bail!("用法：souba get <键>，例如 souba get vegas.fast");
            };
            if cur.show(key).is_none() {
                anyhow::bail!("未知的参数键 {key:?}，可用键见 souba settings");
            }
            println!("{key} = {}（默认 {}）", shown(&cur, key), shown(&def, key));
        }
        "settings" => {
            println!("{}{}{}", pad("键", 22), pad("当前值", 18), "默认值");
            for key in KEYS {
                let now = shown(&cur, key);
                let d = shown(&def, key);
                let mark = if now == d { "" } else { "   ← 已修改" };
                println!("{}{}{d}{mark}", pad(key, 22), pad(&now, 18));
            }
        }
        _ => anyhow::bail!("未知子命令 {cmd:?}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn 默认值符合规格() {
        let s = Settings::default();
        assert_eq!(s.vegas.filter, 12);
        assert_eq!(s.vegas.fast, (144, 169));
        assert_eq!(s.vegas.slow, (576, 676));
        assert_eq!(s.vegas.min_bars, 1330);
        assert_eq!(s.vegas.slope_window, 5);
        assert_eq!(s.vegas.fresh_window, 3);
        assert_eq!(s.scan.sectors, 8);
        assert_eq!(s.scan.k, 20);
        assert_eq!(s.scan.top, 5);
        assert_eq!(s.scan.heat_days, 5);
        assert_eq!(s.scan.w_turnover, 1.0);
        assert_eq!(s.scan.w_change, 1.0);
        assert_eq!(s.ai_model, "");
        s.validate().expect("默认值自己必须合法");
    }

    #[test]
    fn 空库装载等于默认值() {
        assert_eq!(Settings::load(&store()).unwrap(), Settings::default());
    }

    #[test]
    fn 库里改了快隧道装载结果随之变化() {
        let st = store();
        st.set_setting("vegas.fast", "89,144").unwrap();
        let s = Settings::load(&st).unwrap();
        assert_eq!(s.vegas.fast, (89, 144));
        // 其余键仍取默认
        assert_eq!(s.vegas.slow, (576, 676));
        assert_eq!(s.scan.k, 20);
    }

    #[test]
    fn 库里的扫描参数与模型名也装载() {
        let st = store();
        st.set_setting("scan.w_turnover", "2.5").unwrap();
        st.set_setting("scan.top", "3").unwrap();
        st.set_setting("ai.model", "doubao-seed-1-6").unwrap();
        let s = Settings::load(&st).unwrap();
        assert_eq!(s.scan.w_turnover, 2.5);
        assert_eq!(s.scan.top, 3);
        assert_eq!(s.ai_model, "doubao-seed-1-6");
    }

    #[test]
    fn 每个键都能读出并原样写回() {
        let base = Settings::default();
        for key in KEYS {
            let shown = base.show(key).unwrap_or_else(|| panic!("{key} 没有展示值"));
            let mut s = Settings::default();
            s.apply(key, &shown)
                .unwrap_or_else(|e| panic!("{key} 的默认展示值 {shown:?} 写不回去：{e}"));
            assert_eq!(s, base, "{key} 往返后不一致");
        }
    }

    #[test]
    fn 键集合就是规格里的十三个() {
        let mut got = KEYS.to_vec();
        got.sort_unstable();
        let mut want = vec![
            "vegas.filter", "vegas.fast", "vegas.slow", "vegas.min_bars",
            "vegas.slope_window", "vegas.fresh_window",
            "scan.sectors", "scan.k", "scan.top", "scan.heat_days",
            "scan.w_turnover", "scan.w_change", "ai.model",
        ];
        want.sort_unstable();
        assert_eq!(got, want);
    }

    /// 走 set 子命令那条路：先 apply 再 validate，任一失败即拒绝
    fn set(key: &str, value: &str) -> Result<Settings, String> {
        let mut s = Settings::default();
        s.apply(key, value)?;
        s.validate()?;
        Ok(s)
    }

    #[test]
    fn 合法修改被接受() {
        assert_eq!(set("vegas.fast", "89,144").unwrap().vegas.fast, (89, 144));
        assert_eq!(set("vegas.filter", " 21 ").unwrap().vegas.filter, 21);
        assert_eq!(set("scan.w_change", "0").unwrap().scan.w_change, 0.0);
        assert_eq!(set("scan.heat_days", "0").unwrap().scan.heat_days, 0);
        assert_eq!(set("ai.model", "").unwrap().ai_model, "");
    }

    #[test]
    fn 未知键被拒绝() {
        let e = set("vegas.turbo", "1").unwrap_err();
        assert!(e.contains("vegas.turbo"), "错误要指出是哪个键：{e}");
    }

    #[test]
    fn 类型不对被拒绝() {
        assert!(set("vegas.filter", "abc").is_err());
        assert!(set("vegas.filter", "12.5").is_err());
        assert!(set("vegas.filter", "-1").is_err());
        assert!(set("vegas.fast", "144").is_err(), "隧道必须是两个周期");
        assert!(set("vegas.fast", "144,169,200").is_err());
        assert!(set("vegas.fast", "144,x").is_err());
        assert!(set("scan.w_turnover", "很大").is_err());
        assert!(set("scan.w_turnover", "nan").is_err(), "NaN 不是合法权重");
    }

    #[test]
    fn 周期为零被拒绝() {
        for (k, v) in [
            ("vegas.filter", "0"),
            ("vegas.fast", "0,169"),
            ("vegas.slow", "576,0"),
            ("vegas.slope_window", "0"),
            ("vegas.fresh_window", "0"),
            ("scan.sectors", "0"),
            ("scan.k", "0"),
            ("scan.top", "0"),
        ] {
            assert!(set(k, v).is_err(), "{k}={v} 应被拒绝");
        }
    }

    #[test]
    fn 快隧道不快于慢隧道被拒绝() {
        // 按各自较小的那条比：576 >= 576
        assert!(set("vegas.fast", "576,676").is_err());
        assert!(set("vegas.fast", "600,700").is_err());
        // 慢隧道调到比快隧道还快，同样非法
        assert!(set("vegas.slow", "100,120").is_err());
        let e = set("vegas.fast", "600,700").unwrap_err();
        assert!(e.contains("慢隧道"), "错误要说清是隧道顺序问题：{e}");
    }

    #[test]
    fn min_bars小于慢隧道最长周期被拒绝() {
        assert!(set("vegas.min_bars", "675").is_err());
        assert!(set("vegas.min_bars", "676").is_ok(), "等于最长周期允许");
        assert!(set("vegas.min_bars", "0").is_err());
    }

    #[test]
    fn 负权重被拒绝() {
        let e = set("scan.w_turnover", "-0.5").unwrap_err();
        assert!(e.contains("负"), "错误要说清权重不能为负：{e}");
        assert!(set("scan.w_change", "-1").is_err());
    }
}

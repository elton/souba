//! AI 解读：把策略算好的 `Signal` 翻译成一段人话。
//!
//! 三条硬约束：
//! 1. **喂给模型的是结构化 `Signal`，不是原始 K 线** —— 模型负责解释，不负责计算。
//! 2. **关掉思考模式**（`enable_thinking: false`）—— 实测 qwen3.7-flash 一次简单问答
//!    840 个输出 token 里 784 个是 reasoning，全按输出价计费。
//! 3. **输出必须以免责声明开头**。模型没照做就由客户端补上 —— 这条不能靠模型的自觉。

use std::collections::HashMap;

use chrono::NaiveDate;
use serde_json::{Value, json};

use crate::core::strategy::{FacetState, Signal};
use crate::core::symbol::Symbol;
use crate::store::Store;

/// 回答的第一行。少一个字都算这个工具在说谎。
pub const DISCLAIMER: &str = "机器生成的策略分析，不构成投资建议";

const DEFAULT_BASE_URL: &str = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";
const DEFAULT_MODEL: &str = "qwen3.7-flash";

pub const SYSTEM_PROMPT: &str = "\
你是一个把量化策略的计算结果翻译成人话的助手。

策略是 Vegas 隧道：
- EMA12 是过滤器；EMA144/EMA169 组成快隧道；EMA576/EMA676 组成慢隧道。
- 隧道是一条区间带，不是一条线。单根 K 线刺破那条带子不算有效突破。
- 只有价格站上快隧道才谈得上做多；在隧道下方只观察。
- 价格站上还不够，EMA12 也要站上快隧道才算确认，否则是假突破。
- 慢隧道持续向上就持有，转向下就全部离场。

用户给你的是一段 JSON，里面是策略已经算完的结果，字段含义：
- symbol：标的代码。
- stance：主判定，取值 做多 / 观望 / 离场 / 数据不足。\
「数据不足」是说历史 K 线根数不够、慢隧道不可信，不等于看空。
- facets：五个互相独立的维度，各有 label（维度名）、state（看多/中性/看空）、\
detail（这个维度为什么是这个状态）。五个维度分别是：\
趋势＝慢隧道的斜率；共振＝日线与周线是否同向；位置＝最近几根收盘相对快隧道的多数位置；\
确认＝EMA12 是否也站上了快隧道；预备＝价格是否回到隧道内等待。
- adequacy：have 是实际根数，need 是结论可信所需的根数。
- fresh_bars：确认成立至今经过的 K 线根数，越小表示刚刚确立。

要求：
1. 回答的第一行必须原样写：机器生成的策略分析，不构成投资建议
2. 全程用中文，语气平实，不要营销腔。
3. 只解释策略已经算出来的东西：主判定为什么是这个、哪些维度支持它、哪些维度和它相悖、数据够不够。
4. 不要给买卖指令，不要给目标价、止损价或仓位建议，不要预测涨跌幅。
5. 不要自己重算指标，JSON 里没有的数字一个都不要编。
6. 控制在 300 字以内。";

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("未配置 LLM_API_KEY")]
    MissingKey,
    #[error("请求失败：{0}")]
    Http(String),
    #[error("HTTP {0}：{1}")]
    Status(u16, String),
    #[error("响应无法解析：{0}")]
    Parse(String),
}

/// OpenAI 兼容端点的三件套。
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Endpoint {
    /// 纯函数版本，测试直接喂 map。`model_override` 来自 settings 表的 `ai.model`，
    /// 空则退回环境里的 `LLM_MODEL`。
    pub fn resolve(vars: &HashMap<String, String>, model_override: &str) -> Result<Self, AiError> {
        let get = |k: &str| {
            vars.get(k)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let api_key = get("LLM_API_KEY").ok_or(AiError::MissingKey)?;
        let base_url = get("LLM_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let model = match model_override.trim() {
            "" => get("LLM_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            m => m.to_string(),
        };
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        })
    }

    /// 进程环境优先，其次当前目录的 `.env`。
    pub fn from_env(model_override: &str) -> Result<Self, AiError> {
        let mut vars = parse_env(&std::fs::read_to_string(".env").unwrap_or_default());
        for k in ["LLM_BASE_URL", "LLM_API_KEY", "LLM_MODEL"] {
            if let Ok(v) = std::env::var(k)
                && !v.trim().is_empty()
            {
                vars.insert(k.to_string(), v);
            }
        }
        Self::resolve(&vars, model_override)
    }

    pub fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

/// 最小 `.env` 解析：`KEY=VALUE` 逐行，跳过空行与 `#` 注释，去掉成对的引号。
/// 不引 dotenv —— 这个文件只有我自己写，不需要支持多行值与变量展开。
pub fn parse_env(text: &str) -> HashMap<String, String> {
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

fn state_word(s: FacetState) -> &'static str {
    match s {
        FacetState::Bullish => "看多",
        FacetState::Neutral => "中性",
        FacetState::Bearish => "看空",
    }
}

/// `Signal` → user 消息。纯函数，缓存键也算在它的输出上。
pub fn user_prompt(symbol: &Symbol, signal: &Signal) -> String {
    let facets: Vec<Value> = signal
        .facets
        .iter()
        .map(|f| {
            json!({
                "label": f.label,
                "state": state_word(f.state),
                "detail": f.detail,
            })
        })
        .collect();
    let v = json!({
        "symbol": symbol.to_string(),
        "stance": signal.stance.label(),
        "facets": facets,
        "adequacy": {
            "have": signal.adequacy.have,
            "need": signal.adequacy.need,
            "ok": signal.adequacy.ok(),
        },
        "fresh_bars": signal.fresh_bars,
    });
    // 字段顺序由 serde_json 的 Map 保持插入序（默认特性），所以同一个 Signal
    // 每次都得到同样的字节 —— 缓存键靠这一点才稳定
    serde_json::to_string(&v).expect("Signal 的 JSON 化不会失败")
}

/// FNV-1a 64。
///
/// 缓存键不需要密码学强度，而 `sha2` 并不在本项目当前 target 的依赖树里
/// （`Cargo.lock` 里那条是别的 target 拉进来的），为它加一个新依赖不划算。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 缓存键 = 输入 JSON 的哈希 + 日期。同一个 `Signal` 同一天只问一次。
pub fn cache_key(user_json: &str, day: NaiveDate) -> String {
    format!("{:016x}-{day}", fnv1a64(user_json.as_bytes()))
}

/// chat completions 请求体。`enable_thinking` 与 `stream` 都必须是 false。
pub fn request_body(ep: &Endpoint, user_json: &str) -> Value {
    json!({
        "model": ep.model,
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": user_json },
        ],
        "enable_thinking": false,
        "stream": false,
    })
}

/// 从响应里取出正文。取不到就报错，绝不返回空串冒充回答。
pub fn extract_content(body: &str) -> Result<String, AiError> {
    let v: Value = serde_json::from_str(body).map_err(|e| AiError::Parse(e.to_string()))?;
    if let Some(msg) = v.pointer("/error/message").and_then(Value::as_str) {
        return Err(AiError::Parse(msg.to_string()));
    }
    let text = v
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| AiError::Parse(truncate(body, 200)))?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(AiError::Parse("模型返回了空回答".into()));
    }
    Ok(text)
}

fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// 模型没照要求开头就补上。这条不能指望模型的自觉。
pub fn ensure_disclaimer(answer: &str) -> String {
    let t = answer.trim();
    if t.starts_with(DISCLAIMER) {
        t.to_string()
    } else {
        format!("{DISCLAIMER}\n\n{t}")
    }
}

/// 走一次完整流程：查缓存 → 组请求 → 发 → 补免责声明 → 落缓存。
///
/// `send` 是唯一的替换点：真实调用是 `post`，测试里换成计数的假实现。
/// `endpoint` 为 `None` 表示没配 key —— **查缓存排在报这个错之前**，
/// 没配 key 也该能看到今天问过的答案。
pub async fn interpret<F, Fut>(
    store: &Store,
    symbol: &Symbol,
    signal: &Signal,
    endpoint: Option<Endpoint>,
    force: bool,
    day: NaiveDate,
    send: F,
) -> Result<String, AiError>
where
    F: FnOnce(Endpoint, Value) -> Fut,
    Fut: std::future::Future<Output = Result<String, AiError>>,
{
    let user = user_prompt(symbol, signal);
    let key = cache_key(&user, day);
    if !force
        && let Ok(Some(hit)) = store.ai_cached(&key)
    {
        return Ok(hit);
    }
    let ep = endpoint.ok_or(AiError::MissingKey)?;
    let body = request_body(&ep, &user);
    let answer = ensure_disclaimer(&send(ep, body).await?);
    // 缓存写失败不该毁掉一个已经拿到的回答 —— 大不了下次重问
    let _ = store.ai_cache_put(&key, &answer);
    Ok(answer)
}

/// 真实的发送端。
pub async fn post(
    client: &reqwest::Client,
    ep: Endpoint,
    body: Value,
) -> Result<String, AiError> {
    // reqwest 这里没开 json 特性，手动序列化 + 设 Content-Type
    let payload = serde_json::to_string(&body).map_err(|e| AiError::Parse(e.to_string()))?;
    let res = client
        .post(ep.url())
        .header("Authorization", format!("Bearer {}", ep.api_key))
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .map_err(|e| AiError::Http(e.to_string()))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| AiError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(AiError::Status(status.as_u16(), truncate(text.trim(), 200)));
    }
    extract_content(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::strategy::{Adequacy, Facet, Stance};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sig() -> Signal {
        Signal {
            stance: Stance::Long,
            facets: vec![
                Facet {
                    label: "趋势".into(),
                    state: FacetState::Bullish,
                    detail: "慢隧道向上倾斜".into(),
                },
                Facet {
                    label: "确认".into(),
                    state: FacetState::Neutral,
                    detail: "EMA12 仍在快隧道内".into(),
                },
            ],
            adequacy: Adequacy {
                have: 640,
                need: 1330,
            },
            fresh_bars: Some(3),
        }
    }

    fn sym() -> Symbol {
        Symbol::parse("CN:600519").unwrap()
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 18).unwrap()
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn ep() -> Endpoint {
        Endpoint {
            base_url: "https://example.invalid/v1".into(),
            api_key: "k".into(),
            model: "qwen3.7-flash".into(),
        }
    }

    #[test]
    fn system提示词交代了vegas口诀与五个维度() {
        for want in [
            "EMA144", "EMA576", "隧道是一条区间带", "趋势", "共振", "位置", "确认", "预备",
        ] {
            assert!(SYSTEM_PROMPT.contains(want), "system 提示词缺少 {want:?}");
        }
    }

    #[test]
    fn system提示词要求免责声明中文且不给买卖指令() {
        assert!(
            SYSTEM_PROMPT.contains(DISCLAIMER),
            "必须把免责声明原文写进 system，不能只说「加个声明」"
        );
        assert!(SYSTEM_PROMPT.contains("用中文"));
        assert!(SYSTEM_PROMPT.contains("不要给买卖指令"));
        assert!(SYSTEM_PROMPT.contains("目标价"), "止盈止损价也要禁掉");
    }

    #[test]
    fn user提示词是signal的json含全部必需字段() {
        let j = user_prompt(&sym(), &sig());
        let v: Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["symbol"], "CN:600519");
        assert_eq!(v["stance"], "做多");
        assert_eq!(v["adequacy"]["have"], 640);
        assert_eq!(v["adequacy"]["need"], 1330);
        assert_eq!(v["adequacy"]["ok"], false);
        assert_eq!(v["fresh_bars"], 3);
        let f = v["facets"].as_array().unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0]["label"], "趋势");
        assert_eq!(f[0]["state"], "看多");
        assert_eq!(f[0]["detail"], "慢隧道向上倾斜");
        assert_eq!(f[1]["state"], "中性");
    }

    #[test]
    fn 没有确认时fresh_bars是null而不是0() {
        // 0 表示「这一根刚确立」，null 表示「根本没确认」—— 混掉会让模型胡说
        let mut s = sig();
        s.fresh_bars = None;
        let v: Value = serde_json::from_str(&user_prompt(&sym(), &s)).unwrap();
        assert!(v["fresh_bars"].is_null());
    }

    #[test]
    fn user提示词对同一个signal逐字节稳定() {
        assert_eq!(user_prompt(&sym(), &sig()), user_prompt(&sym(), &sig()));
    }

    #[test]
    fn 缓存键随输入与日期变化() {
        let a = user_prompt(&sym(), &sig());
        let mut other = sig();
        other.stance = Stance::Exit;
        let b = user_prompt(&sym(), &other);
        assert_eq!(cache_key(&a, day()), cache_key(&a, day()));
        assert_ne!(cache_key(&a, day()), cache_key(&b, day()), "换了信号要换键");
        let tomorrow = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        assert_ne!(cache_key(&a, day()), cache_key(&a, tomorrow), "换了天要换键");
    }

    #[test]
    fn 缓存键是十六进制加日期() {
        let k = cache_key("x", day());
        assert_eq!(k.len(), 16 + 1 + 10, "键格式应为 16 位十六进制 + - + 日期：{k}");
        assert!(k.ends_with("-2026-09-18"), "{k}");
        assert!(k[..16].chars().all(|c| c.is_ascii_hexdigit()), "{k}");
    }

    #[test]
    fn 请求体关掉思考与流式() {
        let b = request_body(&ep(), "{}");
        assert_eq!(b["model"], "qwen3.7-flash");
        assert_eq!(b["enable_thinking"], false, "思考模式 93% 的输出都是 reasoning，必须关");
        assert_eq!(b["stream"], false);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[0]["content"], SYSTEM_PROMPT);
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m[1]["content"], "{}");
    }

    #[test]
    fn 端点拼出chat_completions() {
        assert_eq!(ep().url(), "https://example.invalid/v1/chat/completions");
        let mut e = ep();
        e.base_url = "https://example.invalid/v1".into();
        assert!(!e.url().contains("//chat"), "末尾斜杠要吃掉，不能拼出双斜杠");
    }

    #[test]
    fn 缺key时报未配置而不是别的() {
        let e = Endpoint::resolve(&HashMap::new(), "").unwrap_err();
        assert!(
            matches!(e, AiError::MissingKey),
            "没有 key 必须是 MissingKey"
        );
        assert!(e.to_string().contains("LLM_API_KEY"), "错误要指名少了哪个键");
    }

    #[test]
    fn 端点取默认值并允许模型覆盖() {
        let mut v = HashMap::new();
        v.insert("LLM_API_KEY".to_string(), "sk-x".to_string());
        let e = Endpoint::resolve(&v, "").unwrap();
        assert_eq!(e.base_url, DEFAULT_BASE_URL);
        assert_eq!(e.model, DEFAULT_MODEL);

        v.insert("LLM_MODEL".to_string(), "qwen-max".to_string());
        v.insert("LLM_BASE_URL".to_string(), "https://x.test/v1/".to_string());
        let e = Endpoint::resolve(&v, "").unwrap();
        assert_eq!(e.model, "qwen-max");
        assert_eq!(e.base_url, "https://x.test/v1", "末尾斜杠要去掉");

        // settings 里的 ai.model 非空时压过 .env
        assert_eq!(Endpoint::resolve(&v, " doubao ").unwrap().model, "doubao");
    }

    #[test]
    fn 空白的key等于没配() {
        let mut v = HashMap::new();
        v.insert("LLM_API_KEY".to_string(), "   ".to_string());
        assert!(matches!(
            Endpoint::resolve(&v, "").unwrap_err(),
            AiError::MissingKey
        ));
    }

    #[test]
    fn env解析跳过注释与空行并去引号() {
        let m = parse_env(
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
        let m = parse_env("LLM_API_KEY=a=b=c\n");
        assert_eq!(m.get("LLM_API_KEY").unwrap(), "a=b=c");
    }

    #[test]
    fn 取出正文并去掉首尾空白() {
        let body = r#"{"choices":[{"message":{"content":"  你好  "}}]}"#;
        assert_eq!(extract_content(body).unwrap(), "你好");
    }

    #[test]
    fn 响应结构不对时报错而不是返回空串() {
        for bad in [
            "不是 JSON",
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{}}]}"#,
            r#"{"choices":[{"message":{"content":"  "}}]}"#,
        ] {
            let e = extract_content(bad).unwrap_err();
            assert!(matches!(e, AiError::Parse(_)), "{bad} 应报解析错误");
        }
    }

    #[test]
    fn 端点返回的错误对象被当成错误() {
        let e = extract_content(r#"{"error":{"message":"Invalid API-key"}}"#).unwrap_err();
        assert!(e.to_string().contains("Invalid API-key"), "{e}");
    }

    #[test]
    fn 免责声明缺失时补上已有时不重复() {
        let 补上的 = ensure_disclaimer("趋势向上。");
        assert!(补上的.starts_with(DISCLAIMER), "{补上的}");
        assert!(补上的.contains("趋势向上。"));

        let 已有的 = format!("{DISCLAIMER}\n\n趋势向上。");
        assert_eq!(ensure_disclaimer(&已有的), 已有的);
        // 模型前面吐了空行也算已有
        assert_eq!(ensure_disclaimer(&format!("\n\n{已有的}\n")), 已有的);
        assert_eq!(ensure_disclaimer(&补上的).matches(DISCLAIMER).count(), 1);
    }

    /// 计数用的假发送端
    fn counting<'a>(
        hits: &'a AtomicUsize,
        answer: &'static str,
    ) -> impl FnOnce(Endpoint, Value) -> std::future::Ready<Result<String, AiError>> + 'a {
        move |_, _| {
            hits.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(answer.to_string()))
        }
    }

    #[tokio::test]
    async fn 首次要发请求回答落缓存并带免责声明() {
        let st = store();
        let hits = AtomicUsize::new(0);
        let got = interpret(&st, &sym(), &sig(), Some(ep()), false, day(), counting(&hits, "趋势向上。"))
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(got.starts_with(DISCLAIMER), "{got}");

        let key = cache_key(&user_prompt(&sym(), &sig()), day());
        assert_eq!(st.ai_cached(&key).unwrap().as_deref(), Some(got.as_str()));
    }

    #[tokio::test]
    async fn 同一信号同一天命中缓存不发请求() {
        let st = store();
        let hits = AtomicUsize::new(0);
        let first = interpret(&st, &sym(), &sig(), Some(ep()), false, day(), counting(&hits, "第一次"))
            .await
            .unwrap();
        let second = interpret(&st, &sym(), &sig(), Some(ep()), false, day(), counting(&hits, "第二次"))
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "第二次不该再打网络");
        assert_eq!(first, second);
        assert!(second.contains("第一次"));
    }

    #[tokio::test]
    async fn force绕过缓存重新问() {
        let st = store();
        let hits = AtomicUsize::new(0);
        interpret(&st, &sym(), &sig(), Some(ep()), false, day(), counting(&hits, "第一次"))
            .await
            .unwrap();
        let again = interpret(&st, &sym(), &sig(), Some(ep()), true, day(), counting(&hits, "第二次"))
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(again.contains("第二次"));
    }

    #[tokio::test]
    async fn 换一天或换信号都重新问() {
        let st = store();
        let hits = AtomicUsize::new(0);
        let tomorrow = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        interpret(&st, &sym(), &sig(), Some(ep()), false, day(), counting(&hits, "今天"))
            .await
            .unwrap();
        interpret(&st, &sym(), &sig(), Some(ep()), false, tomorrow, counting(&hits, "明天"))
            .await
            .unwrap();
        let mut other = sig();
        other.stance = Stance::Exit;
        interpret(&st, &sym(), &other, Some(ep()), false, day(), counting(&hits, "换了信号"))
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn 缓存命中时没配key也能拿到回答没命中才报未配置() {
        let st = store();
        let hits = AtomicUsize::new(0);
        interpret(&st, &sym(), &sig(), Some(ep()), false, day(), counting(&hits, "存好的"))
            .await
            .unwrap();
        let got = interpret(&st, &sym(), &sig(), None, false, day(), counting(&hits, "不该发"))
            .await
            .unwrap();
        assert!(got.contains("存好的"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 换个没缓存过的信号，这时才该报「未配置」
        let mut other = sig();
        other.stance = Stance::Watch;
        let e = interpret(&st, &sym(), &other, None, false, day(), counting(&hits, "不该发"))
            .await
            .unwrap_err();
        assert!(matches!(e, AiError::MissingKey), "{e}");
        assert!(e.to_string().contains("LLM_API_KEY"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn 发送失败原样往上报不panic() {
        let st = store();
        let e = interpret(&st, &sym(), &sig(), Some(ep()), false, day(), |_, _| {
            std::future::ready(Err(AiError::Status(401, "Invalid API-key".into())))
        })
        .await
        .unwrap_err();
        assert!(e.to_string().contains("401"), "{e}");
        // 失败的不该进缓存
        let key = cache_key(&user_prompt(&sym(), &sig()), day());
        assert_eq!(st.ai_cached(&key).unwrap(), None);
    }

    /// 真实问一次。默认不跑：`cargo test -- --ignored 联网`
    #[tokio::test]
    #[ignore = "需要外网与 .env 里的 LLM_API_KEY"]
    async fn 联网冒烟真实解读一次() {
        let ep = Endpoint::from_env("").expect("需要 .env 里的 LLM_API_KEY");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap();
        let body = request_body(&ep, &user_prompt(&sym(), &sig()));
        let raw = post(&client, ep, body).await.expect("端点应可用");
        let answer = ensure_disclaimer(&raw);
        println!("---- 模型回答 ----\n{answer}\n------------------");
        assert!(!raw.trim().is_empty(), "回答不能为空");
        assert!(answer.starts_with(DISCLAIMER));
    }
}

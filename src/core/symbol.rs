use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Market {
    Cn,
    Hk,
    Us,
    Jp,
}

impl Market {
    pub fn as_str(self) -> &'static str {
        match self {
            Market::Cn => "CN",
            Market::Hk => "HK",
            Market::Us => "US",
            Market::Jp => "JP",
        }
    }
}

impl FromStr for Market {
    type Err = SymbolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "CN" => Ok(Market::Cn),
            "HK" => Ok(Market::Hk),
            "US" => Ok(Market::Us),
            "JP" => Ok(Market::Jp),
            other => Err(SymbolError::UnknownMarket(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SymbolError {
    #[error("代码缺少市场前缀，应形如 CN:600519，实际为 {0:?}")]
    MissingMarket(String),
    #[error("未知市场 {0:?}，支持 CN/HK/US/JP")]
    UnknownMarket(String),
    #[error("代码为空")]
    EmptyCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Symbol {
    pub market: Market,
    pub code: String,
}

impl Symbol {
    pub fn parse(raw: &str) -> Result<Self, SymbolError> {
        let (m, c) = raw
            .split_once(':')
            .ok_or_else(|| SymbolError::MissingMarket(raw.to_string()))?;
        let market: Market = m.parse()?;
        let code = c.trim();
        if code.is_empty() {
            return Err(SymbolError::EmptyCode);
        }
        let code = match market {
            // 港股内部统一补足 5 位，各源格式不一
            Market::Hk => format!("{code:0>5}"),
            Market::Us => code.to_ascii_uppercase(),
            _ => code.to_string(),
        };
        Ok(Symbol { market, code })
    }

    /// 生成腾讯 qt.gtimg.cn 的查询代码。
    /// 港股必须带 r_ 前缀 —— 裸 hk 前缀是 15-22 分钟延迟的孪生接口。
    pub fn to_tencent(&self) -> String {
        match self.market {
            Market::Cn => format!("{}{}", cn_prefix(&self.code), self.code),
            Market::Hk => format!("r_hk{}", self.code),
            Market::Us => format!("us{}", self.code),
            Market::Jp => format!("jp{}", self.code),
        }
    }
}

/// A股 交易所由代码段决定：6/9 开头沪市，0/2/3 深市，4/8/92 北交所。
fn cn_prefix(code: &str) -> &'static str {
    // 北交所 920xxx 必须先判 —— 它也是 9 开头，会被沪市分支抢走
    if code.starts_with("92") {
        return "bj";
    }
    match code.as_bytes().first() {
        Some(b'6') | Some(b'9') => "sh",
        Some(b'4') | Some(b'8') => "bj",
        _ => "sz",
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.market.as_str(), self.code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析内部格式() {
        let s = Symbol::parse("CN:600519").unwrap();
        assert_eq!(s.market, Market::Cn);
        assert_eq!(s.code, "600519");
    }

    #[test]
    fn 沪市按代码段推导前缀() {
        assert_eq!(Symbol::parse("CN:600519").unwrap().to_tencent(), "sh600519");
        assert_eq!(Symbol::parse("CN:601318").unwrap().to_tencent(), "sh601318");
        assert_eq!(Symbol::parse("CN:688981").unwrap().to_tencent(), "sh688981");
    }

    #[test]
    fn 深市按代码段推导前缀() {
        assert_eq!(Symbol::parse("CN:000001").unwrap().to_tencent(), "sz000001");
        assert_eq!(Symbol::parse("CN:300750").unwrap().to_tencent(), "sz300750");
    }

    #[test]
    fn 北交所按代码段推导前缀() {
        assert_eq!(Symbol::parse("CN:920819").unwrap().to_tencent(), "bj920819");
    }

    #[test]
    fn 港股必须用r前缀取实时() {
        // 裸 hk00700 是 15-22 分钟延迟的孪生接口，绝不能用
        assert_eq!(Symbol::parse("HK:00700").unwrap().to_tencent(), "r_hk00700");
    }

    #[test]
    fn 港股补足五位前导零() {
        assert_eq!(Symbol::parse("HK:700").unwrap().code, "00700");
    }

    #[test]
    fn 美股大写日股原样() {
        assert_eq!(Symbol::parse("US:aapl").unwrap().to_tencent(), "usAAPL");
        assert_eq!(Symbol::parse("JP:7203").unwrap().to_tencent(), "jp7203");
    }

    #[test]
    fn 往返一致() {
        for raw in ["CN:600519", "HK:00700", "US:AAPL", "JP:7203"] {
            assert_eq!(Symbol::parse(raw).unwrap().to_string(), raw);
        }
    }

    #[test]
    fn 拒绝非法输入() {
        assert!(Symbol::parse("600519").is_err());
        assert!(Symbol::parse("XX:600519").is_err());
        assert!(Symbol::parse("CN:").is_err());
    }
}

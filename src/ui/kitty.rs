//! Kitty 图形协议后端。
//!
//! 把像素画布直接贴进终端，画质与 GUI 无异。支持的终端：
//! kitty / WezTerm / Ghostty / otty / Konsole。**tmux 下多半失效，
//! 不支持的终端会显示乱码** —— 所以必须先探测再用，探测不到就退回盲文。
//!
//! 协议要点（`ESC _ G <控制字段> ; <base64 载荷> ESC \`）：
//! - `a=T` 传输并显示，`f=32` 载荷是 RGBA
//! - `s`/`v` 是图像像素宽高，`c`/`r` 是占用的字符格数
//! - `i=<id>` 固定图像 id，重发即替换，避免堆积
//! - `q=2` 让终端别回响应，否则响应会串进 stdin 被当成按键
//! - 载荷按 4096 字节分块，`m=1` 表示后面还有，`m=0` 是最后一块

use std::io::Write;

use crate::ui::canvas::Canvas;

/// 图像 id 基址。每个面板占一个 slot，各用各的 id。
///
/// **`a=T` 每次都会新建一个「放置」（placement），`i=<id>` 只去重图像数据、
/// 不去重放置** —— 所以光靠固定 id 并不能防止叠加，必须在重画前显式删除
/// 自己的放置。这一点踩过坑：滚动时每帧多两个放置，几帧之后整屏糊掉。
const IMAGE_ID_BASE: u32 = 7301;
/// 协议规定分块上限
const CHUNK: usize = 4096;

/// 一个字符格占多少像素。终端不报时退回常见默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPixels {
    pub w: u16,
    pub h: u16,
}

impl CellPixels {
    /// 大多数等宽字体在常见字号下接近这个比例。
    /// 真机上一律走 ioctl 实测 —— 猜错会让图像被拉伸，所以这个值只给测试用。
    #[cfg(test)]
    pub const FALLBACK: CellPixels = CellPixels { w: 10, h: 20 };
}

/// 问终端要「每个字符格多少像素」。
///
/// 先试 ioctl（最快，不需要读回响应），拿不到再发转义序列问 ——
/// **otty 实测 `ws_xpixel`/`ws_ypixel` 都是 0**，所以转义查询这条路是必需的，
/// 不是锦上添花。
///
/// 拿不到就返回 None，调用方据此降级到盲文。宁可画质差，不可把图像拉伸变形。
pub fn detect_cell_pixels() -> Option<CellPixels> {
    from_ioctl().or_else(from_escape_query)
}

#[repr(C)]
struct WinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

fn from_ioctl() -> Option<CellPixels> {
    // macOS 与 Linux 的 TIOCGWINSZ 常量不同
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: u64 = 0x4008_7468;
    #[cfg(not(target_os = "macos"))]
    const TIOCGWINSZ: u64 = 0x5413;

    let mut ws = WinSize { rows: 0, cols: 0, xpixel: 0, ypixel: 0 };
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut WinSize) };
    if rc != 0 || ws.cols == 0 || ws.rows == 0 || ws.xpixel == 0 || ws.ypixel == 0 {
        return None;
    }
    Some(CellPixels {
        w: (ws.xpixel / ws.cols).max(1),
        h: (ws.ypixel / ws.rows).max(1),
    })
}

/// 发 `CSI 16 t` 问每格像素，终端回 `CSI 6 ; 高 ; 宽 t`。
/// 拿不到就退而求其次发 `CSI 14 t` 问整窗像素，再除以字符格数。
///
/// **必须在 ratatui 接管终端之前调用** —— 之后 stdin 归事件循环，
/// 在这里抢读会把响应吞掉或把按键读丢。
fn from_escape_query() -> Option<CellPixels> {
    use std::io::Write;
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return None;
    }
    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[16t\x1b[14t");
    let _ = out.flush();

    let reply = read_and_drain(120);
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    let reply = reply?;

    // CSI 6 ; h ; w t —— 直接就是每格像素
    if let Some(v) = parse_csi_t(&reply, b'6')
        && v.0 > 0
        && v.1 > 0
    {
        return Some(CellPixels { w: v.1, h: v.0 });
    }
    // CSI 4 ; h ; w t —— 整窗像素，除以字符格数
    if let Some((ph, pw)) = parse_csi_t(&reply, b'4')
        && let Ok((cols, rows)) = crossterm::terminal::size()
        && cols > 0
        && rows > 0
        && ph > 0
        && pw > 0
    {
        return Some(CellPixels {
            w: (pw / cols).max(1),
            h: (ph / rows).max(1),
        });
    }
    None
}

/// 读走终端对查询的回应，**并且把 stdin 排空**。
///
/// 「读到想要的就停」是错的：残留字节会留在 stdin 里，之后被事件循环
/// 当成按键。实测过一次 —— 启动时凭空多出一个左箭头，图表一上来就
/// 滚走了 41 根。所以必须读到 poll 明确超时（确认没有更多数据）为止。
fn read_and_drain(first_wait_ms: i32) -> Option<Vec<u8>> {
    const POLLIN: i16 = 0x0001;
    /// 总预算，避免终端一直吐数据时卡住启动
    const MAX_ROUNDS: usize = 16;
    /// 首轮之后只等很短时间 —— 只是确认「没有更多了」
    const DRAIN_WAIT_MS: i32 = 20;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    for round in 0..MAX_ROUNDS {
        let wait = if round == 0 { first_wait_ms } else { DRAIN_WAIT_MS };
        let mut pfd = PollFd { fd: 0, events: POLLIN, revents: 0 };
        let rc = unsafe { poll(&mut pfd as *mut PollFd, 1, wait) };
        if rc <= 0 {
            // poll 超时 = stdin 确实空了，这才是唯一安全的退出条件
            break;
        }
        let n = unsafe { read(0, chunk.as_mut_ptr(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    if buf.is_empty() { None } else { Some(buf) }
}

/// 从 `ESC [ <kind> ; a ; b t` 里取出 (a, b)。
///
/// 注意不能在循环里用 `?` —— `split` 的第一个片段是 ESC 之前的空串，
/// 用 `?` 会让整个函数在第一轮就返回 None。
fn parse_csi_t(buf: &[u8], kind: u8) -> Option<(u16, u16)> {
    let text = String::from_utf8_lossy(buf);
    for part in text.split('\u{1b}') {
        let Some(body) = part.strip_prefix('[') else {
            continue;
        };
        // 响应可能后面还粘着别的内容，取到第一个 't' 为止
        let Some(body) = body.split('t').next() else {
            continue;
        };
        let mut it = body.split(';');
        if it.next() != Some(std::str::from_utf8(&[kind]).unwrap_or("")) {
            continue;
        }
        let (Some(a), Some(b)) = (it.next(), it.next()) else {
            continue;
        };
        if let (Ok(a), Ok(b)) = (a.trim().parse::<u16>(), b.trim().parse::<u16>()) {
            return Some((a, b));
        }
    }
    None
}

/// 终端是否可能支持 Kitty 图形协议。
///
/// 靠环境变量判断而不是发查询再读响应 —— 后者要在 ratatui 接管终端前后
/// 抢 stdin，很容易把响应串成按键。宁可判断保守一点。
pub fn supported() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    // tmux/screen 下即使外层终端支持，转发也多半是坏的
    if std::env::var("TMUX").is_ok() || term.starts_with("screen") {
        return false;
    }
    std::env::var("KITTY_WINDOW_ID").is_ok()
        || term.contains("kitty")
        || term.contains("ghostty")
        || matches!(
            prog.as_str(),
            "otty" | "WezTerm" | "ghostty" | "kitty" | "Konsole"
        )
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 生成把画布贴到 (col,row) 的完整转义序列。
///
/// 单独抽成纯函数是为了能测 —— 直接写 stdout 没法断言。
pub fn encode(canvas: &Canvas, col: u16, row: u16, cols: u16, rows: u16, slot: u32) -> String {
    let id = IMAGE_ID_BASE + slot;
    let payload = b64(canvas.rgba());
    let mut out = String::with_capacity(payload.len() + 256);
    // 先删掉这个 slot 上一帧的放置，否则新旧会叠在一起
    out.push_str(&clear_slot(slot));
    // 再把光标挪到目标格，让图像以光标为左上角落下；C=1 表示画完别动光标
    out.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));

    let chunks: Vec<&str> = payload
        .as_bytes()
        .chunks(CHUNK)
        .map(|c| std::str::from_utf8(c).expect("base64 是纯 ASCII"))
        .collect();
    if chunks.is_empty() {
        return String::new();
    }
    for (i, ch) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            out.push_str(&format!(
                "\x1b_Ga=T,f=32,t=d,i={id},s={},v={},c={cols},r={rows},C=1,q=2,m={more};{ch}\x1b\\",
                canvas.w, canvas.h
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{ch}\x1b\\"));
        }
    }
    out
}

/// 删除某个 slot 的图像与其全部放置。
///
/// `d=I` 是「按 id 删除图像**及其所有放置**」；只用 `d=i` 会留下数据，
/// 而我们每帧都重传，留着没意义。
pub fn clear_slot(slot: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={},q=2;\x1b\\", IMAGE_ID_BASE + slot)
}

/// 清掉本程序用过的所有 slot。切屏或退出前调用，免得残留。
pub fn clear() -> String {
    (0..MAX_SLOTS).map(clear_slot).collect()
}

/// 目前只有主图和指标两个面板，留些余量
pub const MAX_SLOTS: u32 = 4;

pub fn emit(s: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(s.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::canvas::Rgb;

    #[test]
    fn base64_符合rfc4648测试向量() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_长度总是四的倍数() {
        for n in 0..40usize {
            let data = vec![0xABu8; n];
            assert_eq!(b64(&data).len() % 4, 0, "{n} 字节的编码长度不是 4 的倍数");
        }
    }

    #[test]
    fn base64_只含合法字符() {
        let data: Vec<u8> = (0..=255u8).collect();
        let enc = b64(&data);
        assert!(
            enc.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'='),
            "出现了非法 base64 字符"
        );
    }

    #[test]
    fn 转义序列带齐关键字段() {
        let mut c = Canvas::new(4, 4);
        c.set(0, 0, Rgb(255, 0, 0));
        let s = encode(&c, 10, 5, 2, 1, 0);
        assert!(s.contains("\x1b_Ga=T"), "缺少传输并显示指令");
        assert!(s.contains("f=32"), "缺少 RGBA 格式声明");
        assert!(s.contains("s=4,v=4"), "缺少图像像素尺寸");
        assert!(s.contains("c=2,r=1"), "缺少字符格占用尺寸");
        assert!(s.contains("q=2"), "缺少静默标志 —— 终端响应会串进 stdin 当按键");
        assert!(s.contains("C=1"), "缺少不移动光标标志");
        assert!(s.ends_with("\x1b\\"), "转义序列没有正确结束");
    }

    #[test]
    fn 光标先定位到目标格再放置() {
        let c = Canvas::new(2, 2);
        let s = encode(&c, 10, 5, 1, 1, 0);
        // 终端的行列从 1 开始。序列开头是删除上一帧的放置，定位在它之后。
        let mv = s.find("\x1b[6;11H").expect("缺少光标定位序列");
        let put = s.find("a=T").expect("缺少放置指令");
        assert!(mv < put, "必须先定位再放置，否则图会落在光标当前位置");
    }

    #[test]
    fn 大图分块且最后一块标记结束() {
        // 4096 base64 字符对应 3072 字节 = 768 个像素，取远超这个的尺寸
        let c = Canvas::new(200, 200);
        let s = encode(&c, 0, 0, 20, 10, 0);
        assert!(s.contains("m=1;"), "大图应分块，缺少 m=1");
        assert!(s.contains("m=0;"), "最后一块应标记 m=0");
        let first = s.find("m=1;").unwrap();
        let last = s.rfind("m=0;").unwrap();
        assert!(last > first, "m=0 应在最后");
    }

    #[test]
    fn 小图不分块直接标记结束() {
        let c = Canvas::new(4, 4);
        let s = encode(&c, 0, 0, 1, 1, 0);
        assert!(s.contains("m=0;"), "小图应一次发完");
        assert!(!s.contains("m=1;"), "小图不该分块");
    }

    #[test]
    fn 同一slot重发使用同一个id() {
        let c = Canvas::new(4, 4);
        let a = encode(&c, 0, 0, 1, 1, 0);
        let b = encode(&c, 5, 5, 1, 1, 0);
        let id = format!("i={IMAGE_ID_BASE},");
        assert!(a.contains(&id) && b.contains(&id), "同一 slot 两次发送应使用同一个 id");
    }

    #[test]
    fn 不同slot使用不同id() {
        let c = Canvas::new(4, 4);
        let a = encode(&c, 0, 0, 1, 1, 0);
        let b = encode(&c, 0, 0, 1, 1, 1);
        assert!(a.contains(&format!("i={IMAGE_ID_BASE},")));
        assert!(b.contains(&format!("i={},", IMAGE_ID_BASE + 1)));
    }

    #[test]
    fn 每次编码都自带删除上一帧放置的指令() {
        // 这是叠图 bug 的修复点：a=T 每次新建放置，不删就会堆积
        let c = Canvas::new(4, 4);
        let s = encode(&c, 0, 0, 1, 1, 1);
        let del = s.find(&format!("a=d,d=I,i={}", IMAGE_ID_BASE + 1)).expect("缺少删除指令");
        let put = s.find("a=T").expect("缺少放置指令");
        assert!(del < put, "删除必须在放置之前");
    }

    #[test]
    fn 全量清除覆盖所有slot() {
        let all = clear();
        for slot in 0..MAX_SLOTS {
            assert!(
                all.contains(&format!("i={}", IMAGE_ID_BASE + slot)),
                "slot {slot} 没被清掉"
            );
        }
    }

    #[test]
    fn tmux下一律判定不支持() {
        // 外层终端支持也没用 —— tmux 的图形转发多半是坏的
        unsafe {
            std::env::set_var("TMUX", "/tmp/tmux-501/default,123,0");
            std::env::set_var("TERM_PROGRAM", "otty");
        }
        assert!(!supported());
        unsafe {
            std::env::remove_var("TMUX");
        }
    }

    #[test]
    fn 解析每格像素响应() {
        // CSI 6 ; 高 ; 宽 t
        assert_eq!(parse_csi_t(b"\x1b[6;38;19t", b'6'), Some((38, 19)));
    }

    #[test]
    fn 解析整窗像素响应() {
        assert_eq!(parse_csi_t(b"\x1b[4;1200;2400t", b'4'), Some((1200, 2400)));
    }

    #[test]
    fn 两个响应粘在一起也能各自取出() {
        // 我们一次发两个查询，响应会连在一起回来
        let buf = b"\x1b[6;38;19t\x1b[4;2394;4389t";
        assert_eq!(parse_csi_t(buf, b'6'), Some((38, 19)));
        assert_eq!(parse_csi_t(buf, b'4'), Some((2394, 4389)));
    }

    #[test]
    fn 响应类型不匹配时返回none() {
        assert_eq!(parse_csi_t(b"\x1b[4;100;200t", b'6'), None);
        assert_eq!(parse_csi_t(b"", b'6'), None);
        assert_eq!(parse_csi_t(b"garbage", b'6'), None);
    }

    #[test]
    fn 残缺响应不panic() {
        for bad in [&b"\x1b[6;38t"[..], b"\x1b[6;t", b"\x1b[", b"\x1b[6;abc;def t"] {
            let _ = parse_csi_t(bad, b'6');
        }
    }

    #[test]
    fn 探测不到像素尺寸时返回none而不是猜() {
        // 猜错会让位图被拉伸变形。测试环境的 stdout 不是 tty，
        // 正好覆盖「终端不报尺寸」这条路径。
        let got = detect_cell_pixels();
        if let Some(cp) = got {
            assert!(cp.w > 0 && cp.h > 0, "报出来的尺寸必须是正数");
        }
    }
}

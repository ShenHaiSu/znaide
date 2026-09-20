/// SSE 增量解析器。关键点:
/// 1. 一条事件(`data: ...` 行)可能被拆在多个 HTTP chunk 里,必须跨 chunk 缓冲,
///    否则后半段(没有 `data:` 前缀)会被丢掉 → 工具 arguments 被截断。
/// 2. 一个中文字符(UTF-8 3 字节)/emoji(4 字节)也可能被 TCP 切开,必须按**字节**
///    缓冲,只解码完整事件。若按每个 chunk 直接 `from_utf8_lossy`,半截汉字会
///    永远变成 `�`(U+FFFD,不可逆),进而污染 `write_file` 落盘文件 → `��现状`。
/// 与协议无关:chat(`/chat/completions`)与 Responses(`/responses`)共用。
pub(crate) struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 送入一块网络数据,返回其中完整事件的 `data:` 负载(已去前缀)。
    /// 事件以空行分隔;兼容 \n / \r\n / \r。未闭合的部分(含半截 UTF-8)留到下次。
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((pos, delim_len)) = find_event_boundary(&self.buf) {
            // drain 含分隔符的完整事件;分隔符是纯 ASCII,不可能切在 UTF-8 多字节中间,
            // 所以这里整段 decode 是安全的(半截汉字一定还在 buf 里等下半段)。
            let event_bytes: Vec<u8> = self.buf.drain(..pos + delim_len).collect();
            let event_body = &event_bytes[..event_bytes.len() - delim_len];
            // 完整事件理论上是合法 UTF-8;若服务端真发了非法字节,lossy 保底不 panic。
            let event = String::from_utf8_lossy(event_body);
            // 换行统一成 \n(事件已完整,跨 chunk 的 \r\n 不会被误拆成两个 \n)
            let event = event.replace("\r\n", "\n").replace('\r', "\n");
            for line in event.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    out.push(rest.trim().to_string());
                }
            }
        }
        out
    }

    /// 流结束时的残余数据(服务端可能不补最后一个空行)
    pub(crate) fn finish(&mut self) -> Vec<String> {
        if self.buf.iter().all(|b| b.is_ascii_whitespace()) {
            self.buf.clear();
            return Vec::new();
        }
        let event_bytes = std::mem::take(&mut self.buf);
        let event = String::from_utf8_lossy(&event_bytes);
        let event = event.replace("\r\n", "\n").replace('\r', "\n");
        let mut out = Vec::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                out.push(rest.trim().to_string());
            }
        }
        out
    }
}

/// 在字节缓冲里找第一个"空行"(两个连续换行)。
/// 返回 (分隔符起点, 分隔符总长)。换行定义: `\r\n`(2B) / `\n`(1B) / `\r`(1B)。
/// 刻意在"尾部半截"时返回 None 等下个 chunk:
/// - buf 以单个 `\n`/`\r\n` 结尾 → 可能是 `\n\n` 的前半,等。
/// - buf 以单个 `\r` 结尾 → 可能是 `\r\n` 的前半,等(避免把 `\r\n` 撕成 `\n`+`\n`)。
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        // 第一个换行
        let lb1 = if i + 1 < buf.len() && buf[i] == b'\r' && buf[i + 1] == b'\n' {
            2
        } else if buf[i] == b'\n' || buf[i] == b'\r' {
            // 尾部单个 \r:可能是跨 chunk 的 \r\n 前半,等下块
            if buf[i] == b'\r' && i + 1 == buf.len() {
                return None;
            }
            1
        } else {
            i += 1;
            continue;
        };
        let j = i + lb1;
        if j >= buf.len() {
            // 第一个换行正好在尾部:后半还没到,等
            return None;
        }
        // 第二个换行紧跟?
        if j + 1 < buf.len() && buf[j] == b'\r' && buf[j + 1] == b'\n' {
            return Some((i, lb1 + 2));
        } else if buf[j] == b'\n' || buf[j] == b'\r' {
            // 第二个是尾部 \r:可能是 `\r\n` 前半,但空行本身已确定(两个换行已齐),
            // 先按 1 字节 `\r` 结算;若下块以 `\n` 开头,那个 `\n` 会成下个事件的
            // 前导空行被忽略,不会误判(比等下块更及时,finish() 也能兜底)。
            return Some((i, lb1 + 1));
        } else {
            // 不是空行,从第一个换行之后继续找
            i = j;
            continue;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_chunks_no_replacement_char() {
        // 回归:中文 3 字节被 TCP 从中间切开,旧实现按 chunk lossy 会产出 � 并污染文件。
        // "中" = E4 B8 AD,切成 E4 | B8 AD。
        let payload = "data: {\"delta\":\"中文测试\"}\n\n";
        let bytes = payload.as_bytes();
        // 在"中"的第一个字节后切(一定切在多字节中间)
        let cut = payload.find("中文").unwrap() + 1;
        let mut p = SseParser::new();
        let first = p.push(&bytes[..cut]);
        assert!(first.is_empty(), "半截事件不应产出: {first:?}");
        let rest = p.push(&bytes[cut..]);
        assert_eq!(rest.len(), 1);
        assert!(rest[0].contains("中文测试"), "got: {:?}", rest[0]);
        assert!(!rest[0].contains('�'), "绝不允许出现 U+FFFD: {:?}", rest[0]);
    }

    #[test]
    fn utf8_split_byte_by_byte() {
        // 极端:逐字节喂,任何切分下都不应出现 �,最终还能拼出完整中文。
        let payload = "data: {\"delta\":\"现状良好\"}\n\n";
        let mut p = SseParser::new();
        let mut out = Vec::new();
        for b in payload.as_bytes() {
            out.extend(p.push(std::slice::from_ref(b)));
        }
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("现状良好"), "got: {:?}", out[0]);
        assert!(!out.iter().any(|s| s.contains('�')));
    }

    #[test]
    fn emoji_split_across_chunks() {
        // emoji 4 字节(F0 9F 98 80)同样不能被切坏
        let payload = "data: {\"delta\":\"😀😀\"}\n\n";
        let bytes = payload.as_bytes();
        let cut = payload.find("😀").unwrap() + 2; // 切在 4 字节中间
        let mut p = SseParser::new();
        assert!(p.push(&bytes[..cut]).is_empty());
        let rest = p.push(&bytes[cut..]);
        assert_eq!(rest.len(), 1);
        assert!(rest[0].contains("😀😀"));
        assert!(!rest[0].contains('�'));
    }

    #[test]
    fn crlf_split_across_chunks_no_false_boundary() {
        // 跨 chunk 的 \r\n 不能被误拆成两个 \n(旧实现按 chunk normalize 会误判空行)。
        let mut p = SseParser::new();
        // "data: a\r" + "\n\r\n" = 完整事件 "data: a\r\n\r\n",不能在第一块就产出
        assert!(p.push(b"data: a\r").is_empty());
        let out = p.push(b"\n\r\n");
        assert_eq!(out, vec!["a"]);
    }

    #[test]
    fn boundary_split_across_chunks() {
        // 空行分隔符本身被切开:\n | \n, \r\n | \r\n
        let mut p = SseParser::new();
        assert!(p.push(b"data: a\n").is_empty());
        assert_eq!(p.push(b"\n"), vec!["a"]);

        let mut p2 = SseParser::new();
        assert!(p2.push(b"data: b\r\n").is_empty());
        assert_eq!(p2.push(b"\r\n"), vec!["b"]);
    }

    #[test]
    fn lone_cr_line_endings() {
        // 老式 \r 换行同样识别
        let mut p = SseParser::new();
        let out = p.push(b"data: x\r\rdata: y\r\r");
        assert_eq!(out, vec!["x", "y"]);
    }
}

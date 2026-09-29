//! 增量 SSE 解析与帧生成（设计文档 §6.1）。
//!
//! 按行切分字节流（`\n`、`\r\n`、`\r` 都视为行结束），跨网络分片的半行保留到下一块。
//! 行结束符都是 ASCII，不会落在 UTF-8 多字节序列中间，所以只在整行上解码即可。

use bytes::Bytes;

/// 一个 SSE 事件
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    /// 多个 `data:` 行以 `\n` 拼接
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    /// 尚未遇到行结束的字节
    line: Vec<u8>,
    /// 上一块以 `\r` 结尾：下一块开头的 `\n` 属于同一个行结束
    pending_cr: bool,
    event: Option<String>,
    data: Vec<String>,
    has_data: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一块字节，返回其中完整的事件
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let mut bytes = chunk;
        if self.pending_cr {
            self.pending_cr = false;
            if let Some(rest) = bytes.strip_prefix(b"\n") {
                bytes = rest;
            }
        }
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' | b'\r' => {
                    self.line.extend_from_slice(&bytes[start..i]);
                    let line = std::mem::take(&mut self.line);
                    self.handle_line(&line, &mut events);
                    if bytes[i] == b'\r' {
                        if i + 1 < bytes.len() {
                            if bytes[i + 1] == b'\n' {
                                i += 1;
                            }
                        } else {
                            self.pending_cr = true;
                        }
                    }
                    i += 1;
                    start = i;
                }
                _ => i += 1,
            }
        }
        self.line.extend_from_slice(&bytes[start..]);
        events
    }

    /// 流结束：没有以空行结束的最后一个事件也交出（宽松处理，部分上游不写结尾空行）
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.handle_line(&line, &mut events);
        }
        self.dispatch(&mut events);
        events
    }

    fn handle_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line[0] == b':' {
            return; // 注释 / keep-alive
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                self.data.push(value.to_string());
                self.has_data = true;
            }
            _ => {} // id / retry 与未知字段不影响转换
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if self.has_data {
            events.push(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        }
        self.event = None;
        self.data.clear();
        self.has_data = false;
    }
}

/// 生成一个 SSE 帧：`event: <name>\ndata: <json>\n\n`（`event` 为 None 时只写 data）
pub fn frame(event: Option<&str>, data: &str) -> Bytes {
    let mut out = String::with_capacity(data.len() + 32);
    if let Some(event) = event {
        out.push_str("event: ");
        out.push_str(event);
        out.push('\n');
    }
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_string),
            data: data.to_string(),
        }
    }

    const STREAM: &str = ": keep-alive\nevent: message_start\ndata: {\"a\":1}\n\ndata: line1\ndata: line2\nid: 7\nretry: 100\n\ndata: [DONE]\n\n";

    #[test]
    fn parses_events_comments_and_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.feed(STREAM.as_bytes());
        assert_eq!(
            events,
            vec![
                ev(Some("message_start"), "{\"a\":1}"),
                ev(None, "line1\nline2"),
                ev(None, "[DONE]"),
            ]
        );
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn any_chunking_yields_the_same_events() {
        let expected = SseParser::new().feed(STREAM.as_bytes());
        let bytes = STREAM.as_bytes();
        for size in 1..bytes.len() {
            let mut parser = SseParser::new();
            let mut got = Vec::new();
            for chunk in bytes.chunks(size) {
                got.extend(parser.feed(chunk));
            }
            got.extend(parser.finish());
            assert_eq!(got, expected, "chunk size {size}");
        }
    }

    #[test]
    fn crlf_and_cr_line_endings_and_split_crlf() {
        let text = "event: x\r\ndata: 1\r\n\r\nevent: y\rdata: 2\r\r";
        let expected = vec![ev(Some("x"), "1"), ev(Some("y"), "2")];
        assert_eq!(SseParser::new().feed(text.as_bytes()), expected);
        for size in 1..text.len() {
            let mut parser = SseParser::new();
            let mut got = Vec::new();
            for chunk in text.as_bytes().chunks(size) {
                got.extend(parser.feed(chunk));
            }
            assert_eq!(got, expected, "chunk size {size}");
        }
    }

    #[test]
    fn multibyte_utf8_split_across_chunks() {
        let text = "data: 你好，世界\n\n";
        let bytes = text.as_bytes();
        for split in 1..bytes.len() {
            let mut parser = SseParser::new();
            let mut got = parser.feed(&bytes[..split]);
            got.extend(parser.feed(&bytes[split..]));
            assert_eq!(got, vec![ev(None, "你好，世界")], "split at {split}");
        }
    }

    #[test]
    fn unterminated_last_event_is_flushed_on_finish() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: tail").is_empty());
        assert_eq!(parser.finish(), vec![ev(None, "tail")]);
    }

    #[test]
    fn frame_format() {
        assert_eq!(
            frame(Some("ping"), "{\"type\":\"ping\"}"),
            Bytes::from("event: ping\ndata: {\"type\":\"ping\"}\n\n")
        );
        assert_eq!(frame(None, "a\nb"), Bytes::from("data: a\ndata: b\n\n"));
        let mut parser = SseParser::new();
        assert_eq!(parser.feed(&frame(None, "a\nb")), vec![ev(None, "a\nb")]);
    }
}

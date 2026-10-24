//! OpenAI ⇄ Anthropic 协议转换：让反代同时吃 `/v1/chat/completions`。
//!
//! 上游只有 Anthropic 协议端点（`zcode.z.ai/…/anthropic/v1/messages`），所以兼容层做两头翻译：
//!   请求：OpenAI messages（含 tool 往返/多模态 parts）→ Anthropic content blocks
//!   非流：Anthropic JSON → chat.completion
//!   流：Anthropic SSE 事件流 → OpenAI chat.completion.chunk SSE（含 [DONE]）
//! 换号/绑定/冷却/用量统计全部复用原管线 —— 转换只发生在进出边界。
//!
//! 取舍：tool 的流式增量与少数冷门字段（logprobi、n>1、response_format）透传不译；
//! 错误体保持 Anthropic 形（绝大多数客户端只读 error.message）。

use serde_json::{json, Value};
use std::io::Read;

/// OpenAI 请求 → Anthropic 请求体。返回可直接与现有 external_body 拼接的 JSON。
pub fn body_to_anthropic(raw: &[u8]) -> Result<Value, String> {
    let v: Value = serde_json::from_slice(raw).map_err(|e| format!("请求体不是合法 JSON：{e}"))?;
    let obj = v.as_object().ok_or("请求体必须是 JSON 对象")?;
    let msgs = obj
        .get("messages")
        .and_then(|m| m.as_array())
        .filter(|a| !a.is_empty())
        .ok_or("messages 不能为空")?;

    let mut system: Vec<String> = Vec::new();
    if let Some(s) = obj.get("system") {
        push_text(&mut system, s);
    }
    let mut out: Vec<Value> = Vec::new();
    for m in msgs {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        match role {
            "system" | "developer" => push_text(&mut system, m.get("content").unwrap_or(&Value::Null)),
            "tool" | "function" => {
                let mut blocks = Vec::new();
                let id = m
                    .get("tool_call_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool_0")
                    .to_string();
                let mut tr = json!({"type": "tool_result", "tool_use_id": id});
                let c = m.get("content").cloned().unwrap_or(json!(""));
                match c {
                    Value::String(s) => tr["content"] = json!(s),
                    Value::Array(parts) => {
                        let joined: Vec<String> = parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_string))
                            .collect();
                        tr["content"] = json!(joined.join("\n"));
                    }
                    other => tr["content"] = other,
                }
                blocks.push(tr);
                out.push(json!({"role": "user", "content": blocks}));
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(c) = m.get("content") {
                    let mut txt = String::new();
                    push_text(&mut vec![txt.clone()], c); // 探路避免借用：见下方直取
                    match c {
                        Value::String(s) => txt.push_str(s),
                        Value::Array(parts) => {
                            for p in parts {
                                if p.get("type").and_then(Value::as_str) != Some("image_url") {
                                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                                        txt.push_str(t);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    if !txt.trim().is_empty() {
                        blocks.push(json!({"type": "text", "text": txt}));
                    }
                }
                if let Some(calls) = m.get("tool_calls").and_then(|x| x.as_array()) {
                    for call in calls {
                        let f = call.get("function").unwrap_or(&Value::Null);
                        let name = f.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        let args = f
                            .get("arguments")
                            .and_then(Value::as_str)
                            .map(|s| serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({"_raw": s})))
                            .unwrap_or(Value::Object(Default::default()));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.get("id").and_then(Value::as_str).unwrap_or("call_0"),
                            "name": name,
                            "input": args,
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            _ => {
                // user 与未知角色都按 user 收；多模态 parts 里的 image_url 翻成 Anthropic 图像块
                let mut blocks: Vec<Value> = Vec::new();
                let c = m.get("content").cloned().unwrap_or(json!(""));
                match c {
                    Value::String(s) => blocks.push(json!({"type": "text", "text": s})),
                    Value::Array(parts) => {
                        for p in parts {
                            match p.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                                        blocks.push(json!({"type": "text", "text": t}));
                                    }
                                }
                                Some("image_url") => {
                                    if let Some(u) = p.pointer("/image_url/url").and_then(Value::as_str) {
                                        if u.starts_with("data:") {
                                            // data:image/png;base64,xxxx
                                            if let Some((meta, b64)) = u.split_once(',') {
                                                let mime = meta
                                                    .strip_prefix("data:")
                                                    .and_then(|m| m.split(';').next())
                                                    .unwrap_or("image/png")
                                                    .to_string();
                                                blocks.push(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": b64}}));
                                            }
                                        } else {
                                            blocks.push(json!({"type": "image", "source": {"type": "url", "url": u}}));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => blocks.push(json!({"type": "text", "text": c.to_string()})),
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                out.push(json!({"role": "user", "content": blocks}));
            }
        }
    }

    let mut anthropic = json!({
        "model": obj.get("model").and_then(Value::as_str).unwrap_or(""),
        "messages": out,
    });
    let map = anthropic.as_object_mut().unwrap();
    if !system.is_empty() {
        map.insert("system".into(), json!(system.join("\n\n")));
    }
    map.insert("max_tokens".into(), json!(obj.get("max_tokens").and_then(Value::as_u64).unwrap_or(8192u64)));
    for k in ["temperature", "top_p", "stream", "user", "metadata"] {
        if let Some(x) = obj.get(k) {
            map.insert(k.into(), x.clone());
        }
    }
    // 思维链开关：优先级为 原生 thinking 对象 > reasoning_effort 档位映射 > 不开思考。
    // 1) 客户端若已带 Anthropic 形 `thinking`（{"type":"enabled","budget_tokens":N}）则原样转发，
    //    上游据此返回 thinking 块，再由 resp_to_openai / StreamTee 翻成 reasoning_content。
    // 2) 仅有 OpenAI 的 `reasoning_effort`（"low/medium/high/max/auto" 之类）时，按档位映射出
    //    Anthropic 预算（见 effort_to_budget）；"none/off/disabled" 则明确不开思考。
    let native_thinking = obj
        .get("thinking")
        .filter(|t| t.get("type").and_then(Value::as_str).is_some())
        .is_some();
    if let Some(t) = obj.get("thinking") {
        if t.get("type").and_then(Value::as_str).is_some() {
            map.insert("thinking".into(), t.clone());
        }
    }
    if !native_thinking {
        let max_tokens = map["max_tokens"].as_u64().unwrap_or(8192);
        if let Some(budget) = effort_to_budget(obj.get("reasoning_effort").and_then(Value::as_str), max_tokens) {
            map.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": budget}));
        }
    }
    if let Some(stop) = obj.get("stop") {
        match stop {
            Value::String(s) => map.insert("stop_sequences".into(), json!([s])),
            Value::Array(a) => map.insert("stop_sequences".into(), json!(a)),
            _ => None,
        };
    }
    Ok(anthropic)
}

/// OpenAI `reasoning_effort` 档位 → Anthropic thinking 的 budget_tokens。
/// 返回 None 表示不开思考：没带档位、或明确 none/off/disabled（大小写不敏感、trim）。
/// 档位基准：low=2048、medium/auto=8192、high=16384、max 及其他已知高档=32768；
/// 未知档位按 medium 兜底。Anthropic 硬约束 budget_tokens < max_tokens：不满足时压到
/// max_tokens-1024（下限 1024，恰为 Anthropic 最小思考预算）。
fn effort_to_budget(effort: Option<&str>, max_tokens: u64) -> Option<u64> {
    let raw = effort?.trim();
    if raw.is_empty() {
        return None;
    }
    let base = match raw.to_ascii_lowercase().as_str() {
        "none" | "off" | "disabled" => return None,
        "low" | "minimal" => 2048,
        "medium" | "auto" => 8192,
        "high" => 16384,
        "max" | "xhigh" | "ultra" | "highest" => 32768,
        _ => 8192,
    };
    Some(if base < max_tokens {
        base
    } else {
        max_tokens.saturating_sub(1024).max(1024)
    })
}

fn push_text(into: &mut Vec<String>, v: &Value) {
    match v {
        Value::String(s) => {
            if !s.trim().is_empty() {
                into.push(s.clone());
            }
        }
        Value::Array(parts) => {
            let t: Vec<String> = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_string))
                .collect();
            let joined = t.join("\n");
            if !joined.trim().is_empty() {
                into.push(joined);
            }
        }
        _ => {}
    }
}

/// Anthropic JSON 响应 → chat.completion。非 OpenAI 上游错误保持原样透传。
pub fn resp_to_openai(resp: &Value) -> Value {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = resp.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(t);
                    }
                }
                // Anthropic 思维块（type=="thinking"，字段 thinking）→ reasoning_content。
                Some("thinking") => {
                    if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                        if !reasoning.is_empty() {
                            reasoning.push_str("\n\n");
                        }
                        reasoning.push_str(t);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(json!({
                        "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                            "arguments": b.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into()),
                        },
                    }));
                }
                _ => {}
            }
        }
    }
    let mut msg = json!({"role": "assistant", "content": if content.is_empty() { Value::Null } else { json!(content) }});
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        msg["tool_calls"] = json!(tool_calls);
    }
    let finish = match resp.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    };
    let (pin, pout, pcacheread) = usage_of(resp);
    let mut out = json!({
        "id": format!("chatcmpl-{}", resp.get("id").and_then(Value::as_str).unwrap_or("relay")),
        "object": "chat.completion",
        "created": now_secs(),
        "model": resp.get("model").and_then(Value::as_str).unwrap_or(""),
        "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
        "usage": {
            "prompt_tokens": pin,
            "completion_tokens": pout,
            "prompt_tokens_details": {"cached_tokens": pcacheread},
            "total_tokens": pin + pout,
        },
    });
    // 上游报错（Anthropic error 形）：塞进 message，别伪装成正常回复
    if let Some(err) = resp.pointer("/error/message").and_then(Value::as_str) {
        out["choices"][0]["message"]["content"] = json!(err);
        out["choices"][0]["finish_reason"] = json!("error");
        out["error"] = json!({"message": err, "type": resp.pointer("/error/type").and_then(Value::as_str).unwrap_or("api_error")});
    }
    out
}

fn usage_of(v: &Value) -> (u64, u64, u64) {
    let u = v.get("usage").cloned().unwrap_or(json!({}));
    (
        u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        u.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0),
    )
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Anthropic SSE 字节流 → OpenAI chunk SSE。逐 `\n\n` 分帧；未收齐的帧留在缓冲。
pub struct StreamTee<R: Read> {
    inner: R,
    inbuf: Vec<u8>,
    outbuf: std::collections::VecDeque<u8>,
    id: String,
    created: i64,
    model: String,
    done: bool,
}

impl<R: Read> StreamTee<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            inbuf: Vec::new(),
            outbuf: std::collections::VecDeque::new(),
            id: String::new(),
            created: now_secs(),
            model: String::new(),
            done: false,
        }
    }
    fn chunk(&mut self, payload: Value) {
        let s = format!("data: {}\n\n", payload);
        self.outbuf.extend(s.as_bytes());
    }
    fn base(&self, choice: Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [choice],
        })
    }
    fn feed(&mut self, ev: &str, data: &Value) {
        match ev {
            "message_start" => {
                if let Some(m) = data.get("message") {
                    self.id = format!(
                        "chatcmpl-{}",
                        m.get("id").and_then(Value::as_str).unwrap_or("relay")
                    );
                    self.model = m.get("model").and_then(Value::as_str).unwrap_or("").to_string();
                }
                self.chunk(self.base(json!({"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null})));
            }
            "content_block_start" => {
                let b = data.get("content_block").unwrap_or(&Value::Null);
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let i = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                    self.chunk(self.base(json!({"index": 0, "delta": {"tool_calls": [{
                        "index": i,
                        "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                        "type": "function",
                        "function": {"name": b.get("name").and_then(Value::as_str).unwrap_or(""), "arguments": ""},
                    }]}, "finish_reason": null})));
                }
            }
            "content_block_delta" => {
                let d = data.get("delta").unwrap_or(&Value::Null);
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = d.get("text").and_then(Value::as_str) {
                            self.chunk(self.base(json!({"index": 0, "delta": {"content": t}, "finish_reason": null})));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(t) = d.get("partial_json").and_then(Value::as_str) {
                            let i = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                            self.chunk(self.base(json!({"index": 0, "delta": {"tool_calls": [{
                                "index": i, "function": {"arguments": t},
                            }]}, "finish_reason": null})));
                        }
                    }
                    // Anthropic 思维链增量（delta.thinking）→ OpenAI chunk 的 reasoning_content。
                    // signature_delta（思维块签名）忽略：客户端不需要、OpenAI 无对应字段。
                    Some("thinking_delta") => {
                        if let Some(t) = d.get("thinking").and_then(Value::as_str) {
                            self.chunk(self.base(json!({"index": 0, "delta": {"reasoning_content": t}, "finish_reason": null})));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                let sr = data
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let finish = match sr {
                    "max_tokens" => "length",
                    "tool_use" => "tool_calls",
                    _ => "stop",
                };
                let mut c = self.base(json!({"index": 0, "delta": {}, "finish_reason": finish}));
                if data.get("usage").is_some() {
                    let (pi, po, pc) = usage_of(data.get("usage").unwrap_or(&Value::Null));
                    c["usage"] = json!({"prompt_tokens": pi, "completion_tokens": po, "prompt_tokens_details": {"cached_tokens": pc}, "total_tokens": pi + po});
                }
                self.chunk(c);
            }
            "message_stop" => {
                self.outbuf.extend(b"data: [DONE]\n\n");
                self.done = true;
            }
            "error" => {
                let msg = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("stream error");
                self.chunk(self.base(json!({"index": 0, "delta": {"content": format!("\n[上游错误] {msg}")}, "finish_reason": "error"})));
                self.outbuf.extend(b"data: [DONE]\n\n");
                self.done = true;
            }
            _ => {}
        }
    }
    fn drain_frames(&mut self) {
        loop {
            let Some(pos) = find_subseq(&self.inbuf, b"\n\n") else { return };
            let frame: Vec<u8> = self.inbuf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&frame).to_string();
            let mut event = String::new();
            let mut data: Value = Value::Null;
            for line in text.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = e.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data:") {
                    data = serde_json::from_str(d.trim()).unwrap_or(Value::Null);
                }
            }
            if !event.is_empty() && data != Value::Null {
                self.feed(&event, &data);
            }
        }
    }
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl<R: Read> Read for StreamTee<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.outbuf.is_empty() && !self.done {
            let mut chunk = [0u8; 4096];
            loop {
                match self.inner.read(&mut chunk) {
                    Ok(0) => {
                        // 上游结束但没等到 message_stop：补 [DONE]，客户端才能收流
                        if !self.done {
                            self.outbuf.extend(b"data: [DONE]\n\n");
                            self.done = true;
                        }
                        break;
                    }
                    Ok(n) => {
                        self.inbuf.extend_from_slice(&chunk[..n]);
                        self.drain_frames();
                        if !self.outbuf.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
        }
        let mut n = 0;
        while let Some(b) = self.outbuf.pop_front() {
            out[n] = b;
            n += 1;
            if n == out.len() {
                break;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_and_user_roles() {
        let raw = r#"{"model":"gpt","messages":[{"role":"system","content":"你是助手"},{"role":"user","content":"你好"}]}"#;
        let a = body_to_anthropic(raw.as_bytes()).unwrap();
        assert_eq!(a["system"], "你是助手");
        assert_eq!(a["messages"][0]["role"], "user");
        assert_eq!(a["messages"][0]["content"][0]["text"], "你好");
        assert_eq!(a["max_tokens"], 8192);
    }

    #[test]
    fn reasoning_effort_maps_to_thinking_budget() {
        // high + 充裕 max_tokens：满额 16384 预算，且 < max_tokens。
        let raw = r#"{"model":"m","reasoning_effort":"high","max_tokens":40000,"messages":[{"role":"user","content":"x"}]}"#;
        let a = body_to_anthropic(raw.as_bytes()).unwrap();
        assert_eq!(a["thinking"]["type"], json!("enabled"));
        assert_eq!(a["thinking"]["budget_tokens"], json!(16384));
        let budget = a["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!(budget < a["max_tokens"].as_u64().unwrap());
        // high + 缺省 max_tokens=8192：预算压到 max_tokens-1024=7168，仍 < max_tokens；档位大写也识别。
        let raw2 = r#"{"model":"m","reasoning_effort":"HIGH","messages":[{"role":"user","content":"x"}]}"#;
        let b = body_to_anthropic(raw2.as_bytes()).unwrap();
        assert_eq!(b["thinking"]["budget_tokens"], json!(7168));
        assert!(b["thinking"]["budget_tokens"].as_u64().unwrap() < b["max_tokens"].as_u64().unwrap());
    }

    #[test]
    fn native_thinking_not_overridden_by_effort() {
        // 原生 thinking 对象优先级高于档位映射：原样透传、不被覆盖。
        let raw = r#"{"model":"m","reasoning_effort":"high","thinking":{"type":"enabled","budget_tokens":1234},"messages":[{"role":"user","content":"x"}]}"#;
        let a = body_to_anthropic(raw.as_bytes()).unwrap();
        assert_eq!(a["thinking"], json!({"type":"enabled","budget_tokens":1234}));
    }

    #[test]
    fn effort_off_or_absent_means_no_thinking() {
        // none/off/disabled（大小写不敏感、带空格）→ 明确不开思考。
        for eff in ["none", " Off ", "DISABLED"] {
            let raw = format!(r#"{{"model":"m","reasoning_effort":"{eff}","messages":[{{"role":"user","content":"x"}}]}}"#);
            let a = body_to_anthropic(raw.as_bytes()).unwrap();
            assert!(a.get("thinking").is_none(), "{eff} 应关闭思考");
        }
        // 既无 thinking 又无 reasoning_effort：行为不变（不开思考）。
        let raw = r#"{"model":"m","messages":[{"role":"user","content":"x"}]}"#;
        let a = body_to_anthropic(raw.as_bytes()).unwrap();
        assert!(a.get("thinking").is_none());
    }

    #[test]
    fn tool_round_trip_blocks() {
        let raw = r#"{"model":"gpt","messages":[
            {"role":"user","content":"几点?"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_9","type":"function","function":{"name":"clock","arguments":"{\"tz\":\"cn\"}"}}]},
            {"role":"tool","tool_call_id":"call_9","content":"2026"}
        ]}"#;
        let a = body_to_anthropic(raw.as_bytes()).unwrap();
        let msgs = a["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "call_9");
        assert_eq!(msgs[1]["content"][0]["input"]["tz"], "cn");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_9");
        assert_eq!(msgs[2]["content"][0]["content"], "2026");
    }

    #[test]
    fn image_url_to_source_block() {
        let raw = r#"{"model":"g","messages":[{"role":"user","content":[{"type":"text","text":"图"},{"type":"image_url","image_url":{"url":"https://x/a.png"}}]}]}"#;
        let a = body_to_anthropic(raw.as_bytes()).unwrap();
        let c = &a["messages"][0]["content"];
        assert_eq!(c[1]["type"], "image");
        assert_eq!(c[1]["source"]["type"], "url");
    }

    #[test]
    fn resp_basic_and_tools() {
        let resp = json!({"id":"msg_1","model":"glm-5.3","content":[
            {"type":"text","text":"结果"},
            {"type":"tool_use","id":"tu_1","name":"f","input":{"a":1}}
        ],"stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":7}});
        let o = resp_to_openai(&resp);
        assert_eq!(o["choices"][0]["message"]["content"], "结果");
        assert_eq!(o["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(o["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(o["usage"]["total_tokens"], 15);
    }

    #[test]
    fn resp_thinking_to_reasoning_content() {
        let resp = json!({"id":"msg_1","model":"glm-5.3","content":[
            {"type":"thinking","thinking":"先想一下","signature":"sig"},
            {"type":"text","text":"结果"}
        ],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}});
        let o = resp_to_openai(&resp);
        assert_eq!(o["choices"][0]["message"]["content"], "结果");
        assert_eq!(o["choices"][0]["message"]["reasoning_content"], "先想一下");
    }

    #[test]
    fn stream_thinking_delta_to_reasoning_content() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"glm-5.3\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"让我想想\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"abc\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"答案\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut tee = StreamTee::new(sse.as_bytes());
        let mut out = Vec::new();
        tee.read_to_end(&mut out).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("\"reasoning_content\":\"让我想想\""), "{text}");
        assert!(text.contains("\"content\":\"答案\""), "{text}");
        // signature_delta 不应产生任何输出
        assert!(!text.contains("abc"), "{text}");
    }

    #[test]
    fn stream_transform_end_to_end() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"glm-5.3\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut tee = StreamTee::new(sse.as_bytes());
        let mut out = Vec::new();
        tee.read_to_end(&mut out).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("\"object\":\"chat.completion.chunk\""), "{text}");
        assert!(text.contains("\"content\":\"hi\""), "{text}");
        assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        // chunk 顺序：role 在最前
        let role_i = text.find("role").unwrap();
        let content_i = text.find("\"content\":\"hi\"").unwrap();
        assert!(role_i < content_i);
    }

    #[test]
    fn stream_error_gets_done() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream down\"}}\n\n",
        );
        let mut tee = StreamTee::new(sse.as_bytes());
        let mut out = Vec::new();
        tee.read_to_end(&mut out).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("upstream down"));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn no_message_stop_still_closes() {
        let sse = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
        let mut tee = StreamTee::new(sse.as_bytes());
        let mut out = Vec::new();
        tee.read_to_end(&mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).ends_with("data: [DONE]\n\n"));
    }
}

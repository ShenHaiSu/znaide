# 02 — Responses 协议详解

> 以 OpenAI Responses API（`POST {base}/responses`）为准；各家"兼容端点"的字段子集/超集差异用"容错解析"兜底（与现有 chat 层的容错哲学一致）。

## 2.1 一句话对照

| | Chat Completions（现状） | Responses（新增） |
|---|---|---|
| 端点 | `POST {base}/chat/completions` | `POST {base}/responses` |
| 请求消息体 | `messages: [{role,content,tool_calls,…}]` | `input: [...]` + 顶层 `instructions`（system） |
| 工具声明 | `tools: [{type:function,function:{name,description,parameters}}]` | `tools: [{type:function,name,description,parameters}]`（`name` 与顶层平级，不包 `function`） |
| 非流式回答 | `choices[0].message{content,tool_calls}` | `output: [{type,…}]` 数组 |
| 流式 | `choices[].delta` + `[DONE]` | `response.*` 系列事件，结束是 `response.completed` |
| 工具结果回传 | `{role:tool,tool_call_id,content}` | `input` 中的 `{type:function_call_output,call_id,output}` |
| usage | `usage{prompt_tokens,completion_tokens}` | `usage{input_tokens,output_tokens,total_tokens}` |
| 本项目内存格式 | `ChatMessage`（不变，见 D4） | 请求前一刻由 `ChatMessage` 转换；返回时转为 `AssistantReply` |

## 2.2 请求体（`build_response_body`）

```jsonc
{
  "model": "gpt-5",
  "instructions": "<system prompt，来自 ChatMessage[0]，若首条是 system>",
  "input": [
    { "role": "user", "content": "把 ~/Downloads 里的 zip 按日期归档" },
    { "role": "assistant", "content": "好的" },
    {
      "type": "function_call",
      "call_id": "call_1",
      "name": "list_directory",
      "arguments": "{\"path\":\"~/Downloads\"}"
    },
    {
      "type": "function_call_output",
      "call_id": "call_1",
      "output": "<工具输出文本>"
    }
  ],
  "tools": [
    {
      "type": "function",
      "name": "list_directory",
      "description": "…",
      "parameters": { "type": "object", "properties": {…}, "required": […] }
    }
  ],
  "stream": true
}
```

字段说明：

- `model`：同 chat，`self.model`。
- `instructions`：只取内存消息中**首条 system**（与 `Session::push_system_prompt` 对应：`messages[0]` 恒为 system）。若首条不是 system 则不发该字段。`compact` 后结构仍是 `system + 摘要(user) + tail`，规则不变。
- `input`：逐条转换（映射表见 04 §4.1）。content 统一为**字符串**。
- `tools`：由 `ToolDef` 转换（`function.name → name`，`function.description → description`，`parameters` 原样透传；见 04 §4.2）。`tools=None` 时不带该键（与 `build_body` 的 `if let Some(tools)` 同构）。
- `stream`：`bool`，与 chat 同构。**不发** `stream_options`（那是 chat 协议的；Responses 的 usage 随 `response.completed` 一起回）。
- 首版**不发**：`previous_response_id`、`store`、`reasoning`、`max_output_tokens`、`temperature`（chat 侧现在也没发，保持对齐；预留见 04 §4.4）。

## 2.3 非流式响应（`parse_response_response`）

```jsonc
{
  "id": "resp_123",
  "status": "completed",
  "output": [
    {
      "type": "message",
      "content": [{ "type": "output_text", "text": "已归档 3 个文件…" }]
    },
    {
      "type": "function_call",
      "call_id": "call_1",
      "name": "list_directory",
      "arguments": "{\"path\":\"~/Downloads\"}"
    },
    { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "…" }] }
  ],
  "usage": { "input_tokens": 120, "output_tokens": 15, "total_tokens": 135 }
}
```

解析规则（容错优先，与 `parse_chat_response` 同哲学）：

1. `output` 数组逐项处理：
   - `type == "message"`：取 `content[]` 中 `type == "output_text"` 的 `text` 拼接 → `AssistantReply.content`。兼容：`content` 为字符串时直接收；`type == "text"` 的项也收（部分兼容端点）。
   - `type == "function_call"`：`{id: call_id, type: "function", function: {name, arguments}}`。`arguments` 为对象时序列化成字符串；缺 `call_id` 时回落 `format!("call_{name}")`；缺 `name`/空名则丢弃该项（与 `value_to_tool_call` 同规则）。
   - `type == "reasoning"`：取 `summary[].text` 拼接 → `reasoning_content`（对齐 chat 语义；`StreamEvent::ReasoningDelta` 照常可用）。
   - 未知 `type`：忽略（向前兼容）。
2. `usage`：`input_tokens → prompt_tokens`，`output_tokens → completion_tokens`；缺失/非数值 → 0（与 `extract_usage` 同规则）。
3. `status`：非 `completed` 时（`failed`/`incomplete`）→ 读 `error` 字段 `bail!`（见 §2.6）；`incomplete`（截断）对齐 chat 的 `finish_reason == "length"` 文案："模型输出超过上下文长度被截断(length)"。

## 2.4 流式事件（`apply_response_event`）

Responses SSE 同样是 `data: <json>\n\n`（`SseParser` 原样复用），但**事件负载结构不同**，且**没有 `[DONE]`**（流结束标志是 `response.completed` 事件；服务端也可能直接断流，此时以已累积内容收尾，与 `sse.finish()` 冲刷对齐）。

| 事件 `type` | 含义 | 处理 |
|---|---|---|
| `response.output_text.delta` | 文本增量，`delta: "…"` | `content.push_str` + `TextDelta` |
| `response.reasoning_summary_text.delta` / `response.reasoning_text.delta` | 思考增量 | `reasoning.push_str` + `ReasoningDelta`（两个名字都认） |
| `response.function_call_arguments.delta` | 工具参数增量，字段：`output_index`、`item_id`、`delta`（字符串片） | `FunctionCallAccumulator::merge(output_index, item_id, delta)`。`name`/`call_id` 在 `response.output_item.added` 里先到 |
| `response.output_item.added` | 新输出项开始，`output_index` + `item:{type:function_call, id/call_id, name, arguments:""}` | 登记槽位（id/name），arguments 从空开始累 |
| `response.output_item.done` | 第 `output_index` 项完成，`item` 含完整体 | 若是 function_call 且累积器对应槽位仍缺 `name`/`id`，用此补齐；message 则无事可做（文本早已随 delta 到达） |
| `response.completed` | 整次响应完成，`response:{usage:{…}}` | 提取 usage；返回 `done=true`（对齐 `[DONE]` 的 `break 'stream`） |
| `response.failed` / `response.incomplete` | 失败 / 截断，`response:{error:{message}}` | `bail!`（`incomplete` 用截断文案，见 §2.3-3） |
| `response.created` / `response.in_progress` / 其他 | 生命周期/未知 | 忽略 |
| 非 JSON 行 | 心跳/注释 | 忽略（与 `apply_stream_line` 的 `Err(_) => Ok(false)` 同规则） |

`output_index` 说明：一次响应里 message（index 0）与 function_call（index 1…）混排，`output_index` 是 output 数组下标。累积器实现：`Vec<AccToolCall>` 按 `output_index` 扩容（与 `ToolCallAccumulator` 按 `index` 扩容逐行对应），`finish()` 时**只收有 name 的 function_call 槽**（message 槽天然无 name，被过滤掉——正好复用"filter 空名"逻辑）。

最小流示例：

```
data: {"type":"response.created","response":{"id":"resp_1"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}

data: {"type":"response.output_text.delta","output_index":0,"delta":"你好"}

data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"{\"path\":"}}

data: {"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"\"/tmp/a\"}"}

data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"/tmp/a\"}"}}

data: {"type":"response.completed","response":{"usage":{"input_tokens":30,"output_tokens":2}}}
```

## 2.5 工具结果回传（请求侧，`chat_to_response_input` 的一部分）

内存：`assistant(tool_calls=[call_1])` → `tool(call_1, output)` 两条 `ChatMessage`。
发请求时翻译为 `input` 中的两项：

```json
[
  { "type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{…}" },
  { "type": "function_call_output", "call_id": "call_1", "output": "<工具输出>" }
]
```

要点：`function_call` 项必须保留（Responses 靠它把 output 与调用关联；只发 output 会 400）。`output` 恒为字符串（工具输出本就是 `String`）。中断占位（`PLACEHOLDER_TOOL_REPLY`）同样如实发送——它就是一条普通 output。

## 2.6 错误语义

| 情形 | chat 现状 | Responses 对齐 |
|---|---|---|
| HTTP 非 2xx | `bail!("模型端点返回 {status}: {截断 500}")` | 同文案（共享 helper） |
| 截断 | `finish_reason == "length"` → `bail!("模型输出超过上下文长度被截断(length)")` | `status == "incomplete"` / `response.incomplete` → 同文案 |
| 失败 | choices 缺失时 `bail!("模型响应缺少 choices[0].message")` | `status == "failed"` → `bail!("模型响应失败: {error.message}")`；`output` 缺失 → `bail!("模型响应缺少 output")`（文案与 chat 版对仗） |
| 参数错误（400） | 透传服务端文本 | 同样透传；另在 `describe_request_error`（session.rs）里加一条 Responses 400 的中文解读（见 06 §6.4） |

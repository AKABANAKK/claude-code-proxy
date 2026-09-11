use std::collections::{BTreeMap, HashMap, HashSet};

use crate::anthropic::sse::encode_sse_event;
use crate::providers::codex::events::{
    classify_event_failure, event_is_success_terminal, is_standard_max_output_tokens_incomplete,
    response_is_incomplete_terminal,
};
use crate::traffic::TrafficCapture;

use super::IncompleteResponsePolicy;
use super::read_rewrite::sanitize_read_args;
use super::reasoning_signature::{PendingReasoning, encode_reasoning_signature};
use super::reducer::{
    BUFFERED_TOOL_MAX_ARGS_BYTES, CodexUsage, FinishMetadata, STOP_END_TURN, STOP_MAX_TOKENS,
    STOP_TOOL_USE, map_codex_usage_to_anthropic, reasoning_input_item,
};
use super::request::{ResponsesContentPart, ResponsesInputItem};

const BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES: usize = 1_024;

enum LiveBlock {
    Text {
        index: usize,
        text: String,
        deferred: bool,
    },
    Tool {
        index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        had_delta: bool,
        buffer_until_done: bool,
        emitted_args: bool,
    },
}

struct LiveWebSearch {
    index: usize,
    result_index: usize,
    id: String,
    query: String,
}

#[derive(Clone)]
struct LiveWebSearchResult {
    title: String,
    url: String,
}

#[derive(Clone, Copy)]
struct LiveThinking {
    output_index: usize,
    anthropic_index: usize,
}

#[derive(Default)]
struct LiveFinishMetadata {
    output_items: BTreeMap<usize, ResponsesInputItem>,
    open_blocks: HashSet<usize>,
    text_overrides: HashMap<usize, String>,
    response_id: Option<String>,
    continuation_eligible: bool,
    finished: bool,
}

pub struct LiveStreamTranslator {
    message_id: String,
    model: String,
    message_started: bool,
    blocks_by_output_index: HashMap<usize, LiveBlock>,
    item_id_to_output_index: HashMap<String, usize>,
    anthropic_index: usize,
    thinking: Option<LiveThinking>,
    reasoning_by_output_index: HashMap<usize, PendingReasoning>,
    saw_tool_use: bool,
    web_search_requests: usize,
    web_searches: Vec<LiveWebSearch>,
    web_search_results: Vec<LiveWebSearchResult>,
    deferred_text: Vec<(usize, String)>,
    semantic_output_started: bool,
    // Seeds Claude Code's live subagent counter until the provider returns
    // authoritative usage in the terminal message_delta.
    estimated_input_tokens: u64,
    incomplete_response_policy: IncompleteResponsePolicy,
    finish_metadata: Option<Box<LiveFinishMetadata>>,
    finished: bool,
}

impl LiveStreamTranslator {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_estimated_input_tokens(message_id, model, 0)
    }

    pub fn with_estimated_input_tokens(
        message_id: impl Into<String>,
        model: impl Into<String>,
        estimated_input_tokens: u64,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            message_started: false,
            blocks_by_output_index: HashMap::new(),
            item_id_to_output_index: HashMap::new(),
            anthropic_index: 0,
            thinking: None,
            reasoning_by_output_index: HashMap::new(),
            saw_tool_use: false,
            web_search_requests: 0,
            web_searches: Vec::new(),
            web_search_results: Vec::new(),
            deferred_text: Vec::new(),
            semantic_output_started: false,
            estimated_input_tokens,
            incomplete_response_policy: IncompleteResponsePolicy::Error,
            finish_metadata: None,
            finished: false,
        }
    }

    pub(crate) fn with_incomplete_response_policy(
        mut self,
        policy: IncompleteResponsePolicy,
    ) -> Self {
        self.incomplete_response_policy = policy;
        self
    }

    pub(crate) fn with_finish_metadata(mut self, enabled: bool) -> Self {
        self.finish_metadata = enabled.then(Box::default);
        self
    }

    pub(crate) fn take_finish_metadata(&mut self) -> Option<FinishMetadata> {
        let metadata = self.finish_metadata.take()?;
        if metadata.finished {
            Some(FinishMetadata {
                continuation_eligible: metadata.continuation_eligible,
                response_id: metadata.response_id,
                output_items: metadata.output_items.into_values().collect(),
            })
        } else {
            None
        }
    }

    pub fn accept(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<Vec<u8>, String> {
        if self.finished {
            return Ok(Vec::new());
        }

        let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = Vec::new();

        let allowed_incomplete = self.incomplete_response_policy
            == IncompleteResponsePolicy::AllowMaxOutputTokens
            && is_standard_max_output_tokens_incomplete(payload);
        if !allowed_incomplete && let Some(failure) = classify_event_failure(payload) {
            self.finish_metadata = None;
            return Err(failure.message);
        }

        match kind {
            "codex.rate_limits" => {
                self.emit_ping(traffic, &mut out);
            }
            "keepalive" | "response.created" | "response.in_progress" => {
                self.emit_ping(traffic, &mut out);
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => {}
            "response.output_item.added" => {
                self.output_item_added(payload, traffic, &mut out);
            }
            "response.reasoning_summary_part.added" => {
                let output_index = output_index(payload);
                if let Some(thinking) = self
                    .thinking
                    .filter(|thinking| thinking.output_index == output_index)
                {
                    self.emit(
                        traffic,
                        &mut out,
                        "content_block_delta",
                        &serde_json::json!({
                            "type": "content_block_delta",
                            "index": thinking.anthropic_index,
                            "delta": {"type": "thinking_delta", "thinking": "\n\n"}
                        }),
                    );
                }
            }
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(payload, traffic, &mut out);
            }
            "response.output_text.delta" => {
                self.text_delta(payload, traffic, &mut out);
            }
            "response.output_text.annotation.added" => {
                self.web_search_annotation(payload);
            }
            "response.function_call_arguments.delta" => {
                self.tool_delta(payload, traffic, &mut out)?;
            }
            "response.function_call_arguments.done" => {
                self.tool_arguments_done(payload);
            }
            "response.output_item.done" => {
                self.output_item_done(payload, traffic, &mut out);
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.finish(payload, traffic, &mut out);
            }
            _ => {}
        }

        Ok(out)
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn has_semantic_output(&self) -> bool {
        self.semantic_output_started
    }

    pub fn ping_chunk(&mut self, traffic: Option<&TrafficCapture>) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.finished {
            self.emit_ping(traffic, &mut out);
        }
        out
    }

    pub fn finish_after_closed_completed_tool_call(
        &mut self,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished || !self.saw_tool_use || !self.blocks_by_output_index.is_empty() {
            return out;
        }
        self.finish_metadata = None;
        self.close_thinking(traffic, &mut out);
        self.ensure_message_start(traffic, &mut out);
        self.emit_finish(STOP_TOOL_USE, None, traffic, &mut out);
        out
    }

    pub fn error_chunk(
        &mut self,
        message: &str,
        error_type: &str,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.finish_metadata = None;
        self.close_open_blocks(traffic, &mut out);
        self.ensure_message_start(traffic, &mut out);
        self.emit(
            traffic,
            &mut out,
            "error",
            &serde_json::json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message,
                }
            }),
        );
        self.finished = true;
        out
    }

    fn ensure_message_start(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        self.emit(
            traffic,
            out,
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.estimated_input_tokens,
                        "output_tokens": 0
                    }
                }
            }),
        );
    }

    fn emit_ping(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        self.ensure_message_start(traffic, out);
        self.emit(traffic, out, "ping", &serde_json::json!({"type": "ping"}));
    }

    fn emit(
        &self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
        event: &str,
        data: &serde_json::Value,
    ) {
        if let Some(traffic) = traffic {
            traffic.write_json_event(
                "050-downstream-event",
                &serde_json::json!({
                    "event": event,
                    "data": data,
                }),
            );
        }
        out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
    }

    fn output_item_added(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some(item) = payload.get("item") else {
            return;
        };
        let output_index = output_index(payload);
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(metadata) = self.finish_metadata.as_mut()
            && matches!(item_type, "message" | "function_call")
        {
            metadata.open_blocks.insert(output_index);
            metadata.text_overrides.remove(&output_index);
        }

        match item_type {
            "reasoning" => {
                self.reasoning_by_output_index
                    .entry(output_index)
                    .or_default()
                    .capture(item);
            }
            "message" => {
                self.close_thinking(traffic, out);
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    self.item_id_to_output_index
                        .insert(id.to_string(), output_index);
                }
                let deferred = !self.web_searches.is_empty();
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Text {
                        index,
                        text: String::new(),
                        deferred,
                    },
                );
                if !deferred {
                    self.ensure_message_start(traffic, out);
                    self.emit(
                        traffic,
                        out,
                        "content_block_start",
                        &serde_json::json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    );
                }
            }
            "function_call" => {
                self.close_thinking(traffic, out);
                self.saw_tool_use = true;
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Tool {
                        index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        had_delta: false,
                        buffer_until_done: name == "Read",
                        emitted_args: false,
                    },
                );
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": {}
                        }
                    }),
                );
            }
            "web_search_call" => {
                self.web_search_requests += 1;
            }
            _ => {}
        }
    }

    fn reasoning_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let output_index = output_index(payload);
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return;
        }
        self.semantic_output_started = true;
        if self.thinking.map(|thinking| thinking.output_index) != Some(output_index) {
            self.close_thinking(traffic, out);
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            self.thinking = Some(LiveThinking {
                output_index,
                anthropic_index: index,
            });
            self.ensure_message_start(traffic, out);
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""}
                }),
            );
        }
        let index = self
            .thinking
            .expect("thinking block was started")
            .anthropic_index;
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "thinking_delta", "thinking": delta}
            }),
        );
    }

    fn text_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        self.close_thinking(traffic, out);
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return;
        }
        self.semantic_output_started = true;

        let addressed_output_index = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or_else(|| {
                payload
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| self.item_id_to_output_index.get(id).copied())
            });
        let output_index = addressed_output_index.unwrap_or(0);

        if !self.blocks_by_output_index.contains_key(&output_index) {
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            let deferred = !self.web_searches.is_empty();
            self.blocks_by_output_index.insert(
                output_index,
                LiveBlock::Text {
                    index,
                    text: String::new(),
                    deferred,
                },
            );
            if !deferred {
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                );
            }
        }

        let Some(LiveBlock::Text {
            index,
            text,
            deferred,
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return;
        };
        if let Some(metadata) = self.finish_metadata.as_mut()
            && metadata.open_blocks.contains(&output_index)
        {
            if addressed_output_index.is_some() {
                if let Some(captured_text) = metadata.text_overrides.get_mut(&output_index) {
                    captured_text.push_str(delta);
                }
            } else {
                metadata
                    .text_overrides
                    .entry(output_index)
                    .or_insert_with(|| text.clone());
            }
        }
        text.push_str(delta);
        if *deferred {
            return;
        }
        let index = *index;
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": delta}
            }),
        );
    }

    fn tool_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let Some(output_index) = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        else {
            return Ok(());
        };
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return Ok(());
        }
        self.semantic_output_started = true;
        let mut repaired_read: Option<(usize, String)> = None;
        let Some(LiveBlock::Tool {
            index,
            call_id,
            name,
            args_accum,
            had_delta,
            buffer_until_done,
            emitted_args,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Ok(());
        };
        args_accum.push_str(delta);
        *had_delta = true;
        if args_accum.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
            self.finish_metadata = None;
        }
        if *buffer_until_done {
            if args_accum.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
                return Err(format!(
                    "Buffered {name} tool arguments exceeded safe limits"
                ));
            }
            if let Some(repaired) =
                repair_whitespace_stalled_read_args(name, args_accum, Some(call_id.as_str()))
            {
                *args_accum = repaired.clone();
                *emitted_args = true;
                repaired_read = Some((*index, repaired));
            }
        } else {
            *emitted_args = true;
            let index = *index;
            self.semantic_output_started = true;
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": delta
                    }
                }),
            );
            return Ok(());
        }
        if let Some((index, repaired)) = repaired_read {
            if let Some(LiveBlock::Tool {
                call_id,
                name,
                args_accum,
                ..
            }) = self.blocks_by_output_index.remove(&output_index)
            {
                self.capture_tool_output(output_index, call_id, name, args_accum);
            }
            if let Some(metadata) = self.finish_metadata.as_mut() {
                metadata.finished = true;
            }
            self.semantic_output_started = true;
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": repaired
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
            self.ensure_message_start(traffic, out);
            self.emit_finish(STOP_TOOL_USE, None, traffic, out);
        }
        Ok(())
    }

    fn tool_arguments_done(&mut self, payload: &serde_json::Value) {
        let Some(output_index) = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        else {
            return;
        };
        let Some(args) = payload.get("arguments").and_then(|v| v.as_str()) else {
            return;
        };
        let Some(LiveBlock::Tool { args_accum, .. }) =
            self.blocks_by_output_index.get_mut(&output_index)
        else {
            return;
        };
        if args_accum.is_empty() {
            *args_accum = args.to_string();
        }
    }

    fn capture_tool_output(
        &mut self,
        output_index: usize,
        call_id: String,
        name: String,
        arguments: String,
    ) {
        if let Some(metadata) = self.finish_metadata.as_mut()
            && metadata.open_blocks.remove(&output_index)
        {
            metadata.output_items.insert(
                output_index,
                ResponsesInputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                },
            );
        }
    }

    fn output_item_done(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let output_index = output_index(payload);
        if let Some(item) = payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            .filter(|item_type| *item_type == "reasoning")
            .and_then(|_| payload.get("item"))
        {
            self.reasoning_by_output_index
                .entry(output_index)
                .or_default()
                .capture(item);
            let had_active_summary = self
                .thinking
                .is_some_and(|thinking| thinking.output_index == output_index);
            self.close_thinking(traffic, out);
            if !had_active_summary {
                self.emit_signature_only_reasoning(output_index, traffic, out);
            }
            return;
        }

        if payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            == Some("web_search_call")
        {
            self.close_thinking(traffic, out);
            self.semantic_output_started = true;
            let item = &payload["item"];
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            let result_index = self.anthropic_index;
            self.anthropic_index += 1;
            let raw_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            self.web_searches.push(LiveWebSearch {
                index,
                result_index,
                id: super::web_search_compat::server_tool_use_id_from_codex_web_search_id(raw_id),
                query: web_search_query(item),
            });
            return;
        }

        let Some(mut state) = self.blocks_by_output_index.remove(&output_index) else {
            return;
        };

        match &mut state {
            LiveBlock::Text {
                index,
                text,
                deferred,
            } => {
                if let Some(metadata) = self.finish_metadata.as_mut()
                    && metadata.open_blocks.remove(&output_index)
                {
                    let captured_text = metadata
                        .text_overrides
                        .remove(&output_index)
                        .unwrap_or_else(|| {
                            if *deferred {
                                text.clone()
                            } else {
                                std::mem::take(text)
                            }
                        });
                    if !captured_text.is_empty() {
                        metadata.output_items.insert(
                            output_index,
                            ResponsesInputItem::Message {
                                role: "assistant".to_string(),
                                content: vec![ResponsesContentPart::OutputText {
                                    text: captured_text,
                                }],
                            },
                        );
                    }
                }
                if *deferred {
                    self.deferred_text.push((*index, std::mem::take(text)));
                } else {
                    self.emit(
                        traffic,
                        out,
                        "content_block_stop",
                        &serde_json::json!({
                            "type": "content_block_stop",
                            "index": index,
                        }),
                    );
                }
            }
            LiveBlock::Tool {
                index,
                name,
                call_id,
                args_accum,
                had_delta,
                buffer_until_done,
                emitted_args,
                ..
            } => {
                self.semantic_output_started = true;
                let final_args = payload
                    .get("item")
                    .and_then(|item| item.get("arguments"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                let metadata_arguments = if self.finish_metadata.is_some()
                    && (payload.get("item").is_none()
                        || (!*had_delta
                            && !*emitted_args
                            && !args_accum.is_empty()
                            && final_args
                                .is_some_and(|arguments| arguments != args_accum.as_str())))
                {
                    Some(args_accum.clone())
                } else {
                    None
                };
                if let Some(final_args) = final_args
                    && (args_accum.is_empty() || (!*had_delta && !*emitted_args))
                {
                    *args_accum = final_args.to_string();
                }
                if !args_accum.is_empty() {
                    *args_accum = sanitize_read_args(name, args_accum, Some(call_id.as_str()));
                    if *buffer_until_done || !*emitted_args {
                        *emitted_args = true;
                        self.emit(
                            traffic,
                            out,
                            "content_block_delta",
                            &serde_json::json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": args_accum
                                }
                            }),
                        );
                    }
                }
                self.emit(
                    traffic,
                    out,
                    "content_block_stop",
                    &serde_json::json!({
                        "type": "content_block_stop",
                        "index": index,
                    }),
                );
                let arguments = match metadata_arguments {
                    Some(arguments) if payload.get("item").is_some() => {
                        sanitize_read_args(name, &arguments, Some(call_id.as_str()))
                    }
                    Some(arguments) => arguments,
                    None => std::mem::take(args_accum),
                };
                self.capture_tool_output(
                    output_index,
                    std::mem::take(call_id),
                    std::mem::take(name),
                    arguments,
                );
            }
        }
    }

    fn web_search_annotation(&mut self, payload: &serde_json::Value) {
        let Some(annotation) = payload.get("annotation") else {
            return;
        };
        if annotation.get("type").and_then(|v| v.as_str()) != Some("url_citation") {
            return;
        }
        let Some(url) = annotation.get("url").and_then(|v| v.as_str()) else {
            return;
        };
        if self
            .web_search_results
            .iter()
            .any(|result| result.url == url)
        {
            return;
        }
        let title = annotation
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        self.web_search_results.push(LiveWebSearchResult {
            title: title.to_string(),
            url: url.to_string(),
        });
    }

    fn emit_web_searches(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let searches = std::mem::take(&mut self.web_searches);
        for search in searches {
            self.ensure_message_start(traffic, out);
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": search.index,
                    "content_block": {
                        "type": "server_tool_use",
                        "id": search.id,
                        "name": "web_search",
                        "input": {}
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": search.index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": serde_json::to_string(&serde_json::json!({"query": search.query})).unwrap_or_default()
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": search.index}),
            );
            let results: Vec<_> = self
                .web_search_results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "type": "web_search_result",
                        "title": result.title,
                        "url": result.url,
                    })
                })
                .collect();
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": search.result_index,
                    "content_block": {
                        "type": "web_search_tool_result",
                        "tool_use_id": search.id,
                        "content": results
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": search.result_index}),
            );
        }

        for (index, text) in std::mem::take(&mut self.deferred_text) {
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""}
                }),
            );
            if !text.is_empty() {
                self.emit(
                    traffic,
                    out,
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                );
            }
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": index}),
            );
        }
    }

    fn finish(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        if let Some(metadata) = self.finish_metadata.as_mut() {
            if metadata.open_blocks.is_empty() {
                metadata.finished = true;
                metadata.continuation_eligible =
                    event_is_success_terminal(payload) && !response_is_incomplete_terminal(payload);
                metadata.response_id = payload
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
            } else {
                self.finish_metadata = None;
            }
        }
        self.close_thinking(traffic, out);
        self.close_open_blocks(traffic, out);
        self.emit_web_searches(traffic, out);
        self.ensure_message_start(traffic, out);
        let usage = payload.get("response").map(parse_codex_usage);
        let stop_reason = if self.incomplete_response_policy
            == IncompleteResponsePolicy::AllowMaxOutputTokens
            && is_standard_max_output_tokens_incomplete(payload)
        {
            STOP_MAX_TOKENS
        } else if self.saw_tool_use {
            STOP_TOOL_USE
        } else {
            STOP_END_TURN
        };
        self.emit_finish(stop_reason, usage, traffic, out);
    }

    fn emit_finish(
        &mut self,
        stop_reason: &str,
        usage: Option<CodexUsage>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let mapped = map_codex_usage_to_anthropic(&usage, Some(self.web_search_requests));
        self.emit(
            traffic,
            out,
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": mapped,
            }),
        );
        self.emit(
            traffic,
            out,
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        );
        self.finished = true;
    }

    fn close_open_blocks(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        self.close_thinking(traffic, out);
        let open: Vec<usize> = self.blocks_by_output_index.keys().copied().collect();
        for output_index in open {
            let Some(state) = self.blocks_by_output_index.remove(&output_index) else {
                continue;
            };
            let index = match state {
                LiveBlock::Text {
                    index,
                    text,
                    deferred: true,
                } => {
                    self.deferred_text.push((index, text));
                    continue;
                }
                LiveBlock::Text { index, .. } => index,
                LiveBlock::Tool { index, .. } => index,
            };
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
        }
    }

    fn emit_signature_only_reasoning(
        &mut self,
        output_index: usize,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some(replay) = self
            .reasoning_by_output_index
            .remove(&output_index)
            .and_then(|pending| pending.replay())
        else {
            return;
        };
        let Some(signature) = encode_reasoning_signature(&replay) else {
            return;
        };
        self.semantic_output_started = true;
        let index = self.anthropic_index;
        self.anthropic_index += 1;
        self.ensure_message_start(traffic, out);
        self.emit(
            traffic,
            out,
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "signature_delta", "signature": signature}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": index,
            }),
        );
        if let Some(metadata) = self.finish_metadata.as_mut() {
            metadata
                .output_items
                .insert(output_index, reasoning_input_item(replay));
        }
    }

    fn close_thinking(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let Some(thinking) = self.thinking.take() else {
            return;
        };
        if let Some(replay) = self
            .reasoning_by_output_index
            .remove(&thinking.output_index)
            .and_then(|pending| pending.replay())
            && let Some(signature) = encode_reasoning_signature(&replay)
        {
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": thinking.anthropic_index,
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            );
            if let Some(metadata) = self.finish_metadata.as_mut() {
                metadata
                    .output_items
                    .insert(thinking.output_index, reasoning_input_item(replay));
            }
        }
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": thinking.anthropic_index,
            }),
        );
    }
}

fn web_search_query(item: &serde_json::Value) -> String {
    let Some(action) = item.get("action") else {
        return String::new();
    };
    action
        .get("query")
        .and_then(|v| v.as_str())
        .or_else(|| {
            action
                .get("queries")
                .and_then(|v| v.as_array())
                .and_then(|queries| queries.iter().find_map(|query| query.as_str()))
        })
        .unwrap_or("")
        .to_string()
}

fn output_index(payload: &serde_json::Value) -> usize {
    payload
        .get("output_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

fn parse_codex_usage(response: &serde_json::Value) -> CodexUsage {
    let usage = match response.get("usage") {
        Some(u) => u,
        None => return CodexUsage::default(),
    };
    CodexUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        input_tokens_details_cached: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        output_tokens_details_reasoning: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
    }
}

fn repair_whitespace_stalled_read_args(
    name: &str,
    args: &str,
    call_id: Option<&str>,
) -> Option<String> {
    if name != "Read" {
        return None;
    }
    let trimmed = args.trim_end();
    let trailing_whitespace = args.len().saturating_sub(trimmed.len());
    if trailing_whitespace < BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES {
        return None;
    }
    parse_read_args_candidate(trimmed, call_id).or_else(|| {
        let with_brace = format!("{trimmed}}}");
        parse_read_args_candidate(&with_brace, call_id)
    })
}

fn parse_read_args_candidate(args: &str, call_id: Option<&str>) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    if !is_valid_read_args(&parsed) {
        return None;
    }
    Some(sanitize_read_args(
        "Read",
        &serde_json::to_string(&parsed).ok()?,
        call_id,
    ))
}

fn is_valid_read_args(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "file_path" | "offset" | "limit" | "pages") {
            return false;
        }
    }
    let Some(file_path) = obj.get("file_path").and_then(|v| v.as_str()) else {
        return false;
    };
    if file_path.is_empty() {
        return false;
    }
    if let Some(offset) = obj.get("offset").and_then(|v| v.as_i64())
        && offset < 0
    {
        return false;
    }
    if let Some(limit) = obj.get("limit").and_then(|v| v.as_i64())
        && limit <= 0
    {
        return false;
    }
    if obj.get("offset").is_some_and(|v| !v.is_i64()) {
        return false;
    }
    if obj.get("limit").is_some_and(|v| !v.is_i64()) {
        return false;
    }
    if obj.get("pages").is_some_and(|v| !v.is_string()) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::sse::parse_sse_events;
    use serde_json::json;

    fn render(events: Vec<serde_json::Value>) -> String {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        for event in events {
            out.extend(translator.accept(&event, None).unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    fn assert_finish_metadata_matches_reducer(
        events: Vec<serde_json::Value>,
    ) -> Option<FinishMetadata> {
        let mut ordinary = LiveStreamTranslator::new("msg_metadata", "gpt-5.5");
        let mut collecting =
            LiveStreamTranslator::new("msg_metadata", "gpt-5.5").with_finish_metadata(true);
        let mut upstream = Vec::new();
        for event in events {
            upstream.extend_from_slice(format!("data: {event}\n\n").as_bytes());
            let ordinary_chunk = ordinary.accept(&event, None);
            let collected_chunk = collecting.accept(&event, None);
            assert_eq!(collected_chunk, ordinary_chunk);
            if collected_chunk.is_err() || collecting.is_finished() {
                break;
            }
        }
        let expected = super::super::reducer::finish_metadata_from_upstream(&upstream)
            .ok()
            .flatten();
        let actual = collecting.take_finish_metadata();
        assert!(collecting.finish_metadata.is_none());
        assert!(collecting.take_finish_metadata().is_none());
        assert_eq!(
            actual
                .as_ref()
                .map(|metadata| metadata.continuation_eligible),
            expected
                .as_ref()
                .map(|metadata| metadata.continuation_eligible),
        );
        assert_eq!(
            actual
                .as_ref()
                .and_then(|metadata| metadata.response_id.as_deref()),
            expected
                .as_ref()
                .and_then(|metadata| metadata.response_id.as_deref()),
        );
        assert_eq!(
            actual
                .as_ref()
                .map(|metadata| serde_json::to_value(&metadata.output_items).unwrap()),
            expected
                .as_ref()
                .map(|metadata| serde_json::to_value(&metadata.output_items).unwrap()),
        );
        actual
    }

    fn text_stream_events(text: &str) -> Vec<serde_json::Value> {
        vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "metadata-message"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": text
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "metadata-response", "usage": {"input_tokens": 7, "output_tokens": 3}}
            }),
        ]
    }

    fn tool_stream_events(name: &str, arguments: &str) -> Vec<serde_json::Value> {
        vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "metadata-tool", "name": name}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": arguments
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "arguments": arguments}
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "metadata-response", "usage": {}}
            }),
        ]
    }

    #[test]
    fn finish_metadata_matches_text_and_tool_outputs() {
        for text in ["answer", "", "日本語\n記号：é"] {
            let metadata = assert_finish_metadata_matches_reducer(text_stream_events(text))
                .expect("正常完了の終了情報を保持する");
            assert!(metadata.continuation_eligible);
        }
        for (name, arguments) in [
            ("Bash", "{ \"command\": \"pwd\" }"),
            (
                "Read",
                r#"{"file_path":"/tmp/metadata","offset":2,"pages":""}"#,
            ),
        ] {
            let metadata =
                assert_finish_metadata_matches_reducer(tool_stream_events(name, arguments))
                    .expect("ツールの終了情報を保持する");
            assert!(metadata.continuation_eligible);
        }
    }

    #[test]
    fn finish_metadata_preserves_reasoning_replay_with_and_without_summary() {
        for summary in [None, Some("plan")] {
            let mut events = vec![json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "reasoning", "id": "metadata-reasoning", "encrypted_content": "opaque"}
            })];
            if let Some(summary) = summary {
                events.push(json!({
                    "type": "response.reasoning_summary_text.delta",
                    "output_index": 0,
                    "delta": summary
                }));
            }
            events.extend([
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {"type": "reasoning", "id": "metadata-reasoning"}
                }),
                json!({"type": "response.done", "response": {"id": "metadata-response"}}),
            ]);
            let metadata = assert_finish_metadata_matches_reducer(events).unwrap();
            assert!(matches!(
                metadata.output_items.as_slice(),
                [ResponsesInputItem::Reasoning { id, encrypted_content, .. }]
                    if id == "metadata-reasoning" && encrypted_content == "opaque"
            ));
        }
    }

    #[test]
    fn finish_metadata_orders_outputs_by_upstream_index() {
        let mut events = Vec::new();
        for (output_index, text) in [(1, "second"), (0, "first")] {
            for mut event in text_stream_events(text).into_iter().take(3) {
                event["output_index"] = json!(output_index);
                events.push(event);
            }
        }
        events.push(json!({
            "type": "response.completed",
            "response": {"id": "metadata-response"}
        }));
        let metadata = assert_finish_metadata_matches_reducer(events).unwrap();
        let items = serde_json::to_value(metadata.output_items).unwrap();
        assert_eq!(items[0]["content"][0]["text"], "first");
        assert_eq!(items[1]["content"][0]["text"], "second");
    }

    #[test]
    fn finish_metadata_preserves_deferred_web_search_text() {
        let mut events = vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "metadata-search"}
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "metadata-search", "action": {"query": "query"}}
            }),
        ];
        for mut event in text_stream_events("answer") {
            if event.get("output_index").is_some() {
                event["output_index"] = json!(1);
            }
            events.push(event);
        }
        let metadata = assert_finish_metadata_matches_reducer(events).unwrap();
        assert_eq!(metadata.output_items.len(), 1);
    }

    #[test]
    fn finish_metadata_ignores_text_without_reducer_addressing() {
        let mut events = text_stream_events("untracked");
        events.remove(0);
        let metadata = assert_finish_metadata_matches_reducer(events).unwrap();
        assert!(metadata.output_items.is_empty());

        let mut events = text_stream_events("untracked");
        events.remove(2);
        events.remove(0);
        let metadata = assert_finish_metadata_matches_reducer(events).unwrap();
        assert!(metadata.output_items.is_empty());

        let mut events = text_stream_events("kept");
        events.insert(
            2,
            json!({"type": "response.output_text.delta", "delta": " ignored"}),
        );
        events.insert(
            3,
            json!({
                "type": "response.output_text.delta",
                "item_id": "metadata-message",
                "delta": " appended"
            }),
        );
        let metadata = assert_finish_metadata_matches_reducer(events).unwrap();
        let items = serde_json::to_value(metadata.output_items).unwrap();
        assert_eq!(items[0]["content"][0]["text"], "kept appended");
    }

    #[test]
    fn finish_metadata_preserves_done_argument_precedence() {
        for (name, earlier, later) in [
            ("Bash", r#"{ "value": 1 }"#, r#"{"value":2}"#),
            (
                "Read",
                r#"{"file_path":"/tmp/first","offset":2,"pages":""}"#,
                r#"{"file_path":"/tmp/second","offset":3,"pages":""}"#,
            ),
        ] {
            let mut events = tool_stream_events(name, later);
            events[1] = json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "arguments": earlier
            });
            assert_finish_metadata_matches_reducer(events).unwrap();
        }

        let mut events = tool_stream_events("Bash", r#"{"command":"pwd"}"#);
        events.remove(1);
        assert_finish_metadata_matches_reducer(events).unwrap();
    }

    #[test]
    fn finish_metadata_preserves_unsanitized_arguments_when_done_omits_item() {
        let mut events = tool_stream_events(
            "Read",
            r#"{"file_path":"/tmp/metadata","offset":2,"pages":""}"#,
        );
        events[2].as_object_mut().unwrap().remove("item");
        assert_finish_metadata_matches_reducer(events).unwrap();
    }

    #[test]
    fn finish_metadata_rejects_incomplete_or_unclosed_streams() {
        let mut events = text_stream_events("open");
        events.remove(2);
        assert!(assert_finish_metadata_matches_reducer(events).is_none());

        let mut events = text_stream_events("unfinished");
        events.pop();
        assert!(assert_finish_metadata_matches_reducer(events).is_none());

        for terminal in [
            json!({"type": "response.failed", "response": {"error": {"message": "failed"}}}),
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
        ] {
            let mut events = text_stream_events("partial");
            events[3] = terminal;
            assert!(assert_finish_metadata_matches_reducer(events).is_none());
        }
    }

    #[test]
    fn finish_metadata_marks_repaired_read_completion_ineligible() {
        let events = tool_stream_events(
            "Read",
            &format!(
                "{{\"file_path\":\"/tmp/metadata\",\"pages\":\"\"{}",
                " ".repeat(BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES)
            ),
        );
        let metadata = assert_finish_metadata_matches_reducer(events).unwrap();
        assert!(!metadata.continuation_eligible);
    }

    #[test]
    fn finish_metadata_keeps_the_reducer_tool_argument_limit() {
        for length in [
            BUFFERED_TOOL_MAX_ARGS_BYTES,
            BUFFERED_TOOL_MAX_ARGS_BYTES + 1,
        ] {
            let metadata = assert_finish_metadata_matches_reducer(tool_stream_events(
                "Bash",
                &"x".repeat(length),
            ));
            assert_eq!(metadata.is_some(), length == BUFFERED_TOOL_MAX_ARGS_BYTES);
        }
    }

    #[test]
    fn disabled_finish_metadata_does_not_keep_completed_outputs() {
        let mut translator = LiveStreamTranslator::new("msg_metadata", "gpt-5.5")
            .with_finish_metadata(true)
            .with_finish_metadata(false);
        for output_index in 0..128 {
            for mut event in text_stream_events("answer").into_iter().take(3) {
                event["output_index"] = json!(output_index);
                translator.accept(&event, None).unwrap();
                assert!(translator.finish_metadata.is_none());
            }
        }
        translator
            .accept(&json!({"type": "response.completed", "response": {}}), None)
            .unwrap();
        assert!(translator.take_finish_metadata().is_none());
    }

    #[test]
    fn emits_text_delta_before_terminal_event() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let out = translator
            .accept(
                &json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "hello"
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("message_start"));
        assert!(out.contains("content_block_start"));
        assert!(out.contains("text_delta"));
        assert!(out.contains("hello"));
        assert!(!out.contains("message_stop"));
        assert!(translator.has_semantic_output());
    }

    #[test]
    fn estimated_input_is_visible_at_start_and_provider_usage_is_exact_at_finish() {
        let mut translator =
            LiveStreamTranslator::with_estimated_input_tokens("msg_1", "gpt-5.5", 321);

        let started = translator
            .accept(
                &json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "abcdefgh"
                }),
                None,
            )
            .unwrap();
        let started = parse_sse_events(&started)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_start")
            })
            .unwrap();
        assert_eq!(
            started.pointer("/message/usage/input_tokens"),
            Some(&json!(321))
        );

        let finished = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "usage": {"input_tokens": 300, "output_tokens": 9}
                    }
                }),
                None,
            )
            .unwrap();
        let finished = parse_sse_events(&finished)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_delta")
            })
            .unwrap();
        assert_eq!(finished.pointer("/usage/input_tokens"), Some(&json!(300)));
        assert_eq!(finished.pointer("/usage/output_tokens"), Some(&json!(9)));
    }

    #[test]
    fn finishes_text_stream() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_up"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "hello"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "status": "completed", "incomplete_details": null, "usage": {"input_tokens": 2, "output_tokens": 1}}
            }),
        ]);
        assert!(out.contains("content_block_stop"));
        assert!(out.contains("message_delta"));
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(out.contains("message_stop"));
    }

    #[test]
    fn terminal_only_completion_remains_non_semantic() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let out = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed", "incomplete_details": null, "usage": {}}
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(!out.contains(r#""stop_reason":"max_tokens""#));
        assert!(!translator.has_semantic_output());
    }

    #[test]
    fn strict_live_translator_rejects_incomplete_response() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let error = translator
            .accept(
                &json!({
                    "type":"response.incomplete",
                    "response": {
                        "status":"incomplete",
                        "incomplete_details":{"reason":"max_output_tokens"}
                    }
                }),
                None,
            )
            .unwrap_err();
        assert!(error.contains("max_output_tokens"));
    }

    #[test]
    fn standard_responses_policy_maps_max_output_tokens_to_message_stop() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-luna")
            .with_incomplete_response_policy(IncompleteResponsePolicy::AllowMaxOutputTokens);
        let out = translator
            .accept(
                &json!({
                    "type":"response.incomplete",
                    "response": {
                        "status":"incomplete",
                        "error":null,
                        "incomplete_details":{"reason":"max_output_tokens"},
                        "usage":{"input_tokens":2,"output_tokens":8}
                    }
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#""stop_reason":"max_tokens""#));
        assert!(out.contains(r#""output_tokens":8"#));
        assert!(out.contains("event: message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn standard_responses_policy_rejects_other_incomplete_reasons() {
        for reason in ["content_filter", "unknown"] {
            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-luna")
                .with_incomplete_response_policy(IncompleteResponsePolicy::AllowMaxOutputTokens);
            assert!(
                translator
                    .accept(
                        &json!({
                            "type":"response.incomplete",
                            "response": {
                                "status":"incomplete",
                                "incomplete_details":{"reason":reason}
                            }
                        }),
                        None,
                    )
                    .is_err(),
                "{reason}"
            );
        }
    }

    #[test]
    fn structural_tool_start_is_not_semantic_until_arguments_arrive() {
        let mut tool = LiveStreamTranslator::new("msg_tool", "gpt-5.5");
        tool.accept(
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
            }),
            None,
        )
        .unwrap();
        assert!(!tool.has_semantic_output());
        tool.accept(
            &json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{}"
            }),
            None,
        )
        .unwrap();
        assert!(tool.has_semantic_output());

        let mut thinking = LiveStreamTranslator::new("msg_thinking", "gpt-5.5");
        thinking
            .accept(
                &json!({
                    "type": "response.reasoning_summary_text.delta",
                    "output_index": 0,
                    "delta": "plan"
                }),
                None,
            )
            .unwrap();
        assert!(thinking.has_semantic_output());

        let mut web_search = LiveStreamTranslator::new("msg_search", "gpt-5.5");
        web_search
            .accept(
                &json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "web_search_call", "id": "ws_1"}
                }),
                None,
            )
            .unwrap();
        assert!(!web_search.has_semantic_output());
        web_search
            .accept(
                &json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "web_search_call",
                        "id": "ws_1",
                        "action": {"query": "claude-code-proxy"}
                    }
                }),
                None,
            )
            .unwrap();
        assert!(web_search.has_semantic_output());
    }

    #[test]
    fn buffers_read_tool_args_until_done() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"file_path\":\"/tmp/a\",\"pages\":\"\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "Read",
                    "arguments": "{\"file_path\":\"/tmp/a\",\"pages\":\"\"}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);
        assert!(out.contains("tool_use"));
        assert!(out.contains("input_json_delta"));
        assert!(out.contains("/tmp/a"));
        assert!(!out.contains("pages"));
    }

    #[test]
    fn repairs_whitespace_stalled_read_args_as_tool_use_finish() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": {"type":"function_call","call_id":"call_1","name":"Read"}
                    }),
                    None,
                )
                .unwrap(),
        );
        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.function_call_arguments.delta",
                        "output_index": 0,
                        "delta": format!("{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}", " ".repeat(1024))
                    }),
                    None,
                )
                .unwrap(),
        );
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains(r#""partial_json":"{\"file_path\":\"/tmp/a\"}""#));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn websocket_compat_can_finish_after_closed_completed_tool_call() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","call_id":"call_1","name":"WebSearch"}
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "arguments": "{\"query\":\"claude-code-proxy github\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type":"function_call",
                    "call_id":"call_1",
                    "name":"WebSearch",
                    "arguments":"{\"query\":\"claude-code-proxy github\"}"
                }
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }
        out.extend(translator.finish_after_closed_completed_tool_call(None));
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("content_block_start"));
        assert!(rendered.contains("input_json_delta"));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(!rendered.contains("event: error"));
    }

    #[test]
    fn emits_web_search_results_from_citations_before_deferred_text() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "ws_1"}
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "action": {"query": "grok reasoning effort"}
                }
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "message", "id": "msg_up"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 1,
                "delta": "See the official docs."
            }),
            json!({
                "type": "response.output_text.annotation.added",
                "annotation": {
                    "type": "url_citation",
                    "title": "Reasoning",
                    "url": "https://docs.x.ai/docs/guides/reasoning"
                }
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {"type": "message"}
            }),
            json!({
                "type": "response.completed",
                "response": {"status": "completed", "usage": {}}
            }),
        ]);

        let tool = out.find("server_tool_use").unwrap();
        let result = out.find("web_search_tool_result").unwrap();
        let text = out.find("See the official docs.").unwrap();
        assert!(tool < result && result < text);
        assert!(out.contains("https://docs.x.ai/docs/guides/reasoning"));
        assert!(out.contains(r#""web_search_requests":1"#));
    }

    #[test]
    fn rate_limit_event_is_progress_telemetry() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let out = translator
            .accept(
                &json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {"limit_reached": true}
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("event: ping"));
        assert!(!out.contains("event: error"));
    }

    #[test]
    fn progress_events_start_message_and_emit_pings() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let first = String::from_utf8(
            translator
                .accept(&json!({"type": "response.created"}), None)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first.matches("event: message_start").count(), 1);
        assert_eq!(first.matches("event: ping").count(), 1);

        let second = String::from_utf8(translator.ping_chunk(None)).unwrap();
        assert!(!second.contains("event: message_start"));
        assert_eq!(second.matches("event: ping").count(), 1);
    }

    #[test]
    fn live_stream_emits_signature_delta_before_thinking_stop() {
        let out = render(vec![
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}
            }),
            json!({
                "type":"response.reasoning_summary_text.delta",
                "output_index":0,
                "delta":"plan"
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1"}
            }),
            json!({
                "type":"response.completed",
                "response":{"id":"resp_1","usage":{}}
            }),
        ]);
        let thinking_delta = out.find(r#""type":"thinking_delta""#).unwrap();
        let signature_delta = out.find(r#""type":"signature_delta""#).unwrap();
        let thinking_stop = out[signature_delta..]
            .find("event: content_block_stop")
            .map(|offset| signature_delta + offset)
            .unwrap();
        assert!(thinking_delta < signature_delta);
        assert!(signature_delta < thinking_stop);
        assert!(out.contains("ccp:codex:v1:"));
    }

    #[test]
    fn signature_only_reasoning_is_semantic_output() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        for event in [
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1"}
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#""type":"signature_delta""#));
        assert!(translator.has_semantic_output());
    }
}

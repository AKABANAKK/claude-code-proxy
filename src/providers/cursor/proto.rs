use prost::Message;

// ---------------------------------------------------------------------------
// Agent client message (request)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct AgentClientMessage {
    #[prost(message, optional, tag = "1")]
    pub run_request: Option<RunRequest>,
    #[prost(message, optional, tag = "2")]
    pub client_heartbeat: Option<ClientHeartbeat>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RunRequest {
    #[prost(message, optional, tag = "1")]
    pub conversation_state: Option<ConversationState>,
    #[prost(message, optional, tag = "2")]
    pub action: Option<Action>,
    #[prost(message, optional, tag = "6")]
    pub mcp_tools: Option<McpTools>,
    #[prost(string, tag = "7")]
    pub conversation_id: String,
    #[prost(message, optional, tag = "8")]
    pub requested_model: Option<CursorModel>,
    #[prost(bool, tag = "9")]
    pub exclude_workspace_context: bool,
    #[prost(message, repeated, tag = "10")]
    pub selected_subagent_models: Vec<CursorModelRequest>,
    #[prost(string, tag = "11")]
    pub conversation_group_id: String,
    #[prost(bool, tag = "12")]
    pub client_supports_inline_images: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationState {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<ConversationMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationMessage {
    #[prost(string, tag = "1")]
    pub role: String,
    #[prost(string, tag = "2")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Action {
    #[prost(message, optional, tag = "1")]
    pub user_message_action: Option<UserMessageAction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    pub user_message: Option<UserMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserMessage {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, tag = "2")]
    pub message_id: String,
    #[prost(message, optional, tag = "3")]
    pub selected_context: Option<SelectedContext>,
    #[prost(string, tag = "4")]
    pub mode: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectedContext {
    #[prost(message, repeated, tag = "1")]
    pub selected_images: Vec<SelectedImage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpTools {
    #[prost(message, repeated, tag = "1")]
    pub tools: Vec<McpTool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpTool {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(string, tag = "3")]
    pub input_schema: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientHeartbeat {}

#[derive(Clone, PartialEq, Message)]
pub struct CursorModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(message, repeated, tag = "2")]
    pub parameters: Vec<ModelParameter>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CursorModelRequest {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(message, repeated, tag = "2")]
    pub parameters: Vec<ModelParameter>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelParameter {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectedImage {
    #[prost(string, tag = "1")]
    pub data: String,
    #[prost(string, tag = "2")]
    pub uuid: String,
    #[prost(string, tag = "3")]
    pub path: String,
    #[prost(string, tag = "4")]
    pub mime_type: String,
}

// ---------------------------------------------------------------------------
// Agent server message (response)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct AgentServerMessage {
    #[prost(message, optional, tag = "1")]
    pub interaction_update: Option<InteractionUpdate>,
    #[prost(message, optional, tag = "2")]
    pub exec_server_message: Option<ExecServerMessage>,
}

// Tag numbers verified against the Cursor Agent bundle's protobuf descriptors
// (2026.10.01-e373342, 12/10/2026): 1 text_delta, 2 tool_call_started,
// 3 tool_call_completed, 4 thinking_delta, 14 turn_ended. The previous mapping
// (thinking 1, text 2, turn_ended 3) read tool_call_started as the model's text,
// which is why a Cursor tool call reached Claude Code as the literal string
// "call_...\nfc_..." instead of a tool_use block.
#[derive(Clone, PartialEq, Message)]
pub struct InteractionUpdate {
    #[prost(message, optional, tag = "1")]
    pub text_delta: Option<TextDelta>,
    #[prost(message, optional, tag = "4")]
    pub thinking_delta: Option<ThinkingDelta>,
    #[prost(message, optional, tag = "14")]
    pub turn_ended: Option<TurnEnded>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ThinkingDelta {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct TextDelta {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct TurnEnded {
    #[prost(uint64, tag = "1")]
    pub input_tokens: u64,
    #[prost(uint64, tag = "2")]
    pub output_tokens: u64,
    #[prost(uint64, tag = "3")]
    pub cache_read_tokens: u64,
    #[prost(uint64, tag = "4")]
    pub cache_write_tokens: u64,
}

// The frame in which Cursor asks the CLIENT to run a tool. Field numbers from
// the same descriptors: 1 id, 2 shell_args, 3 write_args, 7 read_args,
// 14 shell_stream_args, 15 exec_id. Measured live 12/10/2026: a Read arrives as
// read_args{path, tool_call_id} and a Bash as shell_stream_args with the same
// ShellArgs shape.
#[derive(Clone, PartialEq, Message)]
pub struct ExecServerMessage {
    #[prost(uint64, optional, tag = "1")]
    pub id: Option<u64>,
    #[prost(message, optional, tag = "2")]
    pub shell_args: Option<ShellArgs>,
    #[prost(message, optional, tag = "3")]
    pub write_args: Option<WriteArgs>,
    #[prost(message, optional, tag = "7")]
    pub read_args: Option<ReadArgs>,
    #[prost(message, optional, tag = "14")]
    pub shell_stream_args: Option<ShellArgs>,
    #[prost(string, optional, tag = "15")]
    pub exec_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, optional, tag = "2")]
    pub tool_call_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WriteArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub file_text: String,
    #[prost(string, optional, tag = "3")]
    pub tool_call_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellArgs {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, optional, tag = "2")]
    pub working_directory: Option<String>,
    #[prost(uint64, optional, tag = "3")]
    pub timeout: Option<u64>,
    #[prost(string, optional, tag = "4")]
    pub tool_call_id: Option<String>,
}

//! Given a chat conversation, the model will return a chat completion response.
pub mod structured_output;

use super::{openai_post, ApiResponseOrError, Credentials, Usage};
use crate::openai_request_stream;
use derive_builder::Builder;
use futures_util::StreamExt;
use reqwest::Method;
use reqwest_eventsource::{CannotCloneRequestError, Event, EventSource};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use structured_output::{
    ChatCompletionResponseFormatJsonSchema, JsonSchemaStyle, ToolCallFunctionDefinition,
};
use tokio::sync::mpsc::{channel, Receiver, Sender};

/// A full chat completion.
pub type ChatCompletion = ChatCompletionGeneric<ChatCompletionChoice>;

/// A delta chat completion, which is streamed token by token.
pub type ChatCompletionDelta = ChatCompletionGeneric<ChatCompletionChoiceDelta>;

#[derive(Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ChatCompletionGeneric<C> {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<C>,
    pub usage: Option<Usage>,
}

#[derive(Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ChatCompletionChoice {
    pub index: u64,
    pub finish_reason: String,
    pub message: ChatCompletionMessage,
}

#[derive(Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ChatCompletionChoiceDelta {
    pub index: u64,
    pub finish_reason: Option<String>,
    pub delta: ChatCompletionMessageDelta,
}

fn is_none_or_empty_vec<T>(opt: &Option<Vec<T>>) -> bool {
    opt.as_ref().map(|v| v.is_empty()).unwrap_or(true)
}

#[derive(Deserialize, Serialize, Debug, Clone, Eq, PartialEq, Default)]
pub struct ChatCompletionMessage {
    /// The role of the author of this message.
    pub role: ChatCompletionMessageRole,
    /// The contents of the message
    ///
    /// This is always required for all messages, except for when ChatGPT calls
    /// a function.
    pub content: Option<String>,
    /// The name of the user in a multi-user chat
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The function that ChatGPT called. This should be "None" usually, and is returned by ChatGPT and not provided by the developer
    ///
    /// [API Reference](https://platform.openai.com/docs/api-reference/chat/create#chat/create-function_call)
    #[deprecated(note = "Use `tool_calls` instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<ChatCompletionFunctionCall>,
    /// Tool call that this message is responding to.
    /// Required if the role is `Tool`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls that the assistant is requesting to invoke.
    /// Can only be populated if the role is `Assistant`,
    /// otherwise it should be empty.
    #[serde(skip_serializing_if = "is_none_or_empty_vec")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Same as ChatCompletionMessage, but received during a response stream.
#[derive(Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ChatCompletionMessageDelta {
    /// The role of the author of this message.
    pub role: Option<ChatCompletionMessageRole>,
    /// The contents of the message
    pub content: Option<String>,
    /// The name of the user in a multi-user chat
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The function that ChatGPT called
    ///
    /// [API Reference](https://platform.openai.com/docs/api-reference/chat/create#chat/create-function_call)
    #[deprecated(note = "Use `tool_calls` instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<ChatCompletionFunctionCallDelta>,
    /// Tool call that this message is responding to.
    /// Required if the role is `Tool`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls that the assistant is requesting to invoke.
    /// Can only be populated if the role is `Assistant`,
    /// otherwise it should be empty.
    #[serde(skip_serializing_if = "is_none_or_empty_vec")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatCompletionTool {
    Function {
        function: ToolCallFunctionDefinition,
    },
}

impl ChatCompletionTool {
    pub fn new<T: JsonSchema>(strict: Option<bool>) -> Self {
        let function = ToolCallFunctionDefinition::new::<T>(strict);
        ChatCompletionTool::Function { function }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
pub enum ToolChoiceMode {
    None,
    Auto,
    Required,
}

#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
pub struct FunctionChoice {
    /// The name of the function to call.
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FunctionType {
    Function,
}

#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
#[serde(untagged)]
pub enum ToolChoice {
    /// `none` means the model will not call any tool and instead generates a message.
    /// `auto` means the model can pick between generating a message or calling one or more tools.
    /// `required` means the model must call one or more tools.
    Mode(ToolChoiceMode),
    /// The model will call the function with the given name.
    Function {
        /// The type of the tool. Currently, only `function` is supported.
        r#type: FunctionType,
        /// The function that the model called.
        function: FunctionChoice,
    },
}

impl ToolChoice {
    pub fn mode(mode: ToolChoiceMode) -> Self {
        ToolChoice::Mode(mode)
    }
    pub fn function(name: String) -> Self {
        ToolChoice::Function {
            r#type: FunctionType::Function,
            function: FunctionChoice { name },
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    /// The ID of the tool call.
    pub id: String,
    /// The type of the tool. Currently, only `function` is supported.
    pub r#type: FunctionType,
    /// The function that the model called.
    pub function: ToolCallFunction,
}

#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
pub struct ToolCallDelta {
    pub index: i64,
    /// The ID of the tool call.
    pub id: Option<String>,
    /// The type of the tool. Currently, only `function` is supported.
    pub r#type: Option<FunctionType>,
    /// The function that the model called.
    pub function: Option<ToolCallFunction>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
pub struct ToolCallFunction {
    /// The name of the function to call.
    pub name: String,
    /// The arguments to call the function with, as generated by the model in
    /// JSON format.
    /// Note that the model does not always generate valid JSON, and may
    /// hallucinate parameters not defined by your function schema.
    /// Validate the arguments in your code before calling your function.
    pub arguments: String,
}

#[derive(Deserialize, Serialize, Debug, Clone, Eq, PartialEq)]
pub struct ChatCompletionFunctionDefinition {
    /// The name of the function
    pub name: String,
    /// The description of the function
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The parameters of the function formatted in JSON Schema
    /// [API Reference](https://platform.openai.com/docs/api-reference/chat/create#chat/create-parameters)
    /// [See more information about JSON Schema.](https://json-schema.org/understanding-json-schema/)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ChatCompletionFunctionCall {
    /// The name of the function ChatGPT called
    pub name: String,
    /// The arguments that ChatGPT called (formatted in JSON)
    /// [API Reference](https://platform.openai.com/docs/api-reference/chat/create#chat/create-function_call)
    pub arguments: String,
}

/// Same as ChatCompletionFunctionCall, but received during a response stream.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ChatCompletionFunctionCallDelta {
    /// The name of the function ChatGPT called
    pub name: Option<String>,
    /// The arguments that ChatGPT called (formatted in JSON)
    /// [API Reference](https://platform.openai.com/docs/api-reference/chat/create#chat/create-function_call)
    pub arguments: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ChatCompletionMessageRole {
    System,
    User,
    Assistant,
    Function,
    Tool,
    Developer,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ChatCompletionReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Serialize, Builder, Debug, Clone)]
#[builder(derive(Clone, Debug, PartialEq))]
#[builder(pattern = "owned")]
#[builder(name = "ChatCompletionBuilder")]
#[builder(setter(strip_option, into))]
pub struct ChatCompletionRequest {
    /// ID of the model to use. Currently, only `gpt-3.5-turbo`, `gpt-3.5-turbo-0301` and `gpt-4`
    /// are supported.
    model: String,
    /// The messages to generate chat completions for, in the [chat format](https://platform.openai.com/docs/guides/chat/introduction).
    messages: Vec<ChatCompletionMessage>,
    /// Constrains effort on reasoning for (reasoning models)[https://platform.openai.com/docs/guides/reasoning].
    /// Currently supported values are low, medium, and high (Defaults to medium).
    /// Reducing reasoning effort can result in faster responses and fewer tokens used on reasoning in a response.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ChatCompletionReasoningEffort>,
    /// What sampling temperature to use, between 0 and 2. Higher values like 0.8 will make the output more random, while lower values like 0.2 will make it more focused and deterministic.
    ///
    /// We generally recommend altering this or `top_p` but not both.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// An alternative to sampling with temperature, called nucleus sampling, where the model considers the results of the tokens with top_p probability mass. So 0.1 means only the tokens comprising the top 10% probability mass are considered.
    ///
    /// We generally recommend altering this or `temperature` but not both.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    /// How many chat completion choices to generate for each input message.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<u8>,
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Up to 4 sequences where the API will stop generating further tokens.
    #[builder(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    /// This feature is in Beta. If specified, our system will make a best effort to sample deterministically, such that repeated requests with the same seed and parameters should return the same result. Determinism is not guaranteed, and you should refer to the system_fingerprint response parameter to monitor changes in the backend.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    /// The maximum number of tokens allowed for the generated answer. By default, the number of tokens the model can return will be (4096 - prompt tokens).
    #[deprecated(note = "Use max_completion_tokens instead")]
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    /// The maximum number of tokens allowed for the generated answer.
    /// For reasoning models such as o1 and o3-mini, this does not include reasoning tokens.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on whether they appear in the text so far, increasing the model's likelihood to talk about new topics.
    ///
    /// [See more information about frequency and presence penalties.](https://platform.openai.com/docs/api-reference/parameter-details)
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on their existing frequency in the text so far, decreasing the model's likelihood to repeat the same line verbatim.
    ///
    /// [See more information about frequency and presence penalties.](https://platform.openai.com/docs/api-reference/parameter-details)
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
    /// Modify the likelihood of specified tokens appearing in the completion.
    ///
    /// Accepts a json object that maps tokens (specified by their token ID in the tokenizer) to an associated bias value from -100 to 100. Mathematically, the bias is added to the logits generated by the model prior to sampling. The exact effect will vary per model, but values between -1 and 1 should decrease or increase likelihood of selection; values like -100 or 100 should result in a ban or exclusive selection of the relevant token.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    logit_bias: Option<HashMap<String, f32>>,
    /// A unique identifier representing your end-user, which can help OpenAI to monitor and detect abuse. [Learn more](https://platform.openai.com/docs/guides/safety-best-practices/end-user-ids).
    #[builder(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    user: String,
    /// A list of tools the model may call. Currently, only functions are supported as a tool. Use this to provide a list of functions the model may generate JSON inputs for. A max of 128 functions are supported.
    #[builder(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatCompletionTool>,
    /// Controls which (if any) tool is called by the model.
    /// `none` means the model will not call any tool and instead generates a message.
    /// `auto` means the model can pick between generating a message or calling one or more tools.
    /// `required` means the model must call one or more tools.
    /// Specifying a particular tool via `{"type": "function", "function": {"name": "my_function"}}` forces the model to call that tool.
    ///
    /// `none` is the default when no tools are present. `auto` is the default if tools are present.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    /// Whether to enable parallel function calling during tool use.
    /// Defaults to true.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    /// Describe functions that ChatGPT can call
    /// The latest models of ChatGPT support function calling, which allows you to define functions that can be called from the prompt.
    /// For example, you can define a function called "get_weather" that returns the weather in a given city
    ///
    /// [Function calling API Reference](https://platform.openai.com/docs/api-reference/chat/create#chat/create-functions)
    /// [See more information about function calling in ChatGPT.](https://platform.openai.com/docs/guides/gpt/function-calling)
    #[deprecated(note = "Use tools instead")]
    #[builder(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    functions: Vec<ChatCompletionFunctionDefinition>,
    /// A string or object of the function to call
    ///
    /// Controls how the model responds to function calls
    ///
    /// - "none" means the model does not call a function, and responds to the end-user.
    /// - "auto" means the model can pick between an end-user or calling a function.
    /// - Specifying a particular function via {"name":\ "my_function"} forces the model to call that function.
    ///
    /// "none" is the default when no functions are present. "auto" is the default if functions are present.
    #[deprecated(note = "Use tool_choice instead")]
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<Value>,
    /// An object specifying the format that the model must output. Compatible with GPT-4 Turbo and all GPT-3.5 Turbo models newer than gpt-3.5-turbo-1106.
    /// Setting to { "type": "json_object" } enables JSON mode, which guarantees the message the model generates is valid JSON.
    /// Important: when using JSON mode, you must also instruct the model to produce JSON yourself via a system or user message. Without this, the model may generate an unending stream of whitespace until the generation reaches the token limit, resulting in a long-running and seemingly "stuck" request. Also note that the message content may be partially cut off if finish_reason="length", which indicates the generation exceeded max_tokens or the conversation exceeded the max context length.
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ChatCompletionResponseFormat>,
    /// The credentials to use for this request.
    #[serde(skip_serializing)]
    #[builder(default)]
    credentials: Option<Credentials>,
    /// Parameters unique to the Venice API.
    /// https://docs.venice.ai/api-reference/api-spec
    #[builder(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    venice_parameters: Option<VeniceParameters>,
}

#[derive(Serialize, Debug, Clone, Eq, PartialEq)]
pub struct VeniceParameters {
    pub include_venice_system_prompt: bool,
}

#[derive(Serialize, Debug, Clone, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatCompletionResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        json_schema: ChatCompletionResponseFormatJsonSchema,
    },
}

impl ChatCompletionResponseFormat {
    pub fn text() -> Self {
        ChatCompletionResponseFormat::Text
    }
    pub fn json_object() -> Self {
        ChatCompletionResponseFormat::JsonObject
    }
    pub fn json_schema<T: JsonSchema>(strict: bool, json_style: JsonSchemaStyle) -> Self {
        let json_schema = ChatCompletionResponseFormatJsonSchema::new::<T>(strict, json_style);
        ChatCompletionResponseFormat::JsonSchema { json_schema }
    }
}

impl<C> ChatCompletionGeneric<C> {
    pub fn builder(
        model: &str,
        messages: impl Into<Vec<ChatCompletionMessage>>,
    ) -> ChatCompletionBuilder {
        ChatCompletionBuilder::create_empty()
            .model(model)
            .messages(messages)
    }
}

impl ChatCompletion {
    pub async fn create(request: ChatCompletionRequest) -> ApiResponseOrError<Self> {
        let credentials_opt = request.credentials.clone();
        openai_post("chat/completions", &request, credentials_opt).await
    }
}

impl ChatCompletionDelta {
    pub async fn create(
        request: ChatCompletionRequest,
    ) -> Result<Receiver<Self>, CannotCloneRequestError> {
        let credentials_opt = request.credentials.clone();
        let stream = openai_request_stream(
            Method::POST,
            "chat/completions",
            |r| r.json(&request),
            credentials_opt,
        )
        .await?;
        let (tx, rx) = channel::<Self>(32);
        tokio::spawn(forward_deserialized_chat_response_stream(stream, tx));
        Ok(rx)
    }

    /// Merges the input delta completion into `self`.
    pub fn merge(
        &mut self,
        other: ChatCompletionDelta,
    ) -> Result<(), ChatCompletionDeltaMergeError> {
        if other.id.ne(&self.id) {
            return Err(ChatCompletionDeltaMergeError::DifferentCompletionIds);
        }
        for other_choice in other.choices.iter() {
            for choice in self.choices.iter_mut() {
                if choice.index != other_choice.index {
                    continue;
                }
                choice.merge(other_choice)?;
            }
        }
        Ok(())
    }
}

impl ChatCompletionChoiceDelta {
    pub fn merge(
        &mut self,
        other: &ChatCompletionChoiceDelta,
    ) -> Result<(), ChatCompletionDeltaMergeError> {
        if self.index != other.index {
            return Err(ChatCompletionDeltaMergeError::DifferentCompletionChoiceIndices);
        }
        if self.delta.role.is_none() {
            if let Some(other_role) = other.delta.role {
                // Set role to other_role.
                self.delta.role = Some(other_role);
            }
        }
        if self.delta.name.is_none() {
            if let Some(other_name) = &other.delta.name {
                // Set name to other_name.
                self.delta.name = Some(other_name.clone());
            }
        }
        // Merge contents.
        match self.delta.content.as_mut() {
            Some(content) => {
                match &other.delta.content {
                    Some(other_content) => {
                        // Push other content into this one.
                        content.push_str(other_content)
                    }
                    None => {}
                }
            }
            None => {
                match &other.delta.content {
                    Some(other_content) => {
                        // Set this content to other content.
                        self.delta.content = Some(other_content.clone());
                    }
                    None => {}
                }
            }
        };

        // merge function calls
        // function call names are concatenated
        // arguments are merged by concatenating them
        #[allow(deprecated)]
        match self.delta.function_call.as_mut() {
            Some(function_call) => {
                match &other.delta.function_call {
                    Some(other_function_call) => {
                        // push the arguments string of the other function call into this one
                        match (&mut function_call.arguments, &other_function_call.arguments) {
                            (Some(function_call), Some(other_function_call)) => {
                                function_call.push_str(&other_function_call);
                            }
                            (None, Some(other_function_call)) => {
                                function_call.arguments = Some(other_function_call.clone());
                            }
                            _ => {}
                        }
                    }
                    None => {}
                }
            }
            None => {
                match &other.delta.function_call {
                    Some(other_function_call) => {
                        // Set this content to other content.
                        self.delta.function_call = Some(other_function_call.clone());
                    }
                    None => {}
                }
            }
        };
        Ok(())
    }
}

impl From<ChatCompletionDelta> for ChatCompletion {
    fn from(delta: ChatCompletionDelta) -> Self {
        #[allow(deprecated)]
        ChatCompletion {
            id: delta.id,
            object: delta.object,
            created: delta.created,
            model: delta.model,
            usage: delta.usage,
            choices: delta
                .choices
                .iter()
                .map(|choice| ChatCompletionChoice {
                    index: choice.index,
                    finish_reason: clone_default_unwrapped_option_string(&choice.finish_reason),
                    message: ChatCompletionMessage {
                        role: choice
                            .delta
                            .role
                            .unwrap_or_else(|| ChatCompletionMessageRole::System),
                        content: choice.delta.content.clone(),
                        name: choice.delta.name.clone(),
                        function_call: choice.delta.function_call.clone().map(|f| f.into()),
                        tool_call_id: None,
                        tool_calls: Some(Vec::new()),
                    },
                })
                .collect(),
        }
    }
}

impl From<ChatCompletionFunctionCallDelta> for ChatCompletionFunctionCall {
    fn from(delta: ChatCompletionFunctionCallDelta) -> Self {
        ChatCompletionFunctionCall {
            name: delta.name.unwrap_or("".to_string()),
            arguments: delta.arguments.unwrap_or_default(),
        }
    }
}

#[derive(Debug)]
pub enum ChatCompletionDeltaMergeError {
    DifferentCompletionIds,
    DifferentCompletionChoiceIndices,
    FunctionCallArgumentTypeMismatch,
}

impl std::fmt::Display for ChatCompletionDeltaMergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatCompletionDeltaMergeError::DifferentCompletionIds => {
                f.write_str("Different completion IDs")
            }
            ChatCompletionDeltaMergeError::DifferentCompletionChoiceIndices => {
                f.write_str("Different completion choice indices")
            }
            ChatCompletionDeltaMergeError::FunctionCallArgumentTypeMismatch => {
                f.write_str("Function call argument type mismatch")
            }
        }
    }
}

impl std::error::Error for ChatCompletionDeltaMergeError {}

async fn forward_deserialized_chat_response_stream(
    mut stream: EventSource,
    tx: Sender<ChatCompletionDelta>,
) -> anyhow::Result<()> {
    while let Some(event) = stream.next().await {
        let event = event?;
        match event {
            Event::Message(event) => {
                let completion = serde_json::from_str::<ChatCompletionDelta>(&event.data)?;
                tx.send(completion).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

impl ChatCompletionBuilder {
    pub async fn create(self) -> ApiResponseOrError<ChatCompletion> {
        ChatCompletion::create(self.build().unwrap()).await
    }

    pub async fn create_stream(
        mut self,
    ) -> Result<Receiver<ChatCompletionDelta>, CannotCloneRequestError> {
        self.stream = Some(Some(true));
        ChatCompletionDelta::create(self.build().unwrap()).await
    }
}

fn clone_default_unwrapped_option_string(string: &Option<String>) -> String {
    match string {
        Some(value) => value.clone(),
        None => "".to_string(),
    }
}

impl Default for ChatCompletionMessageRole {
    fn default() -> Self {
        Self::User
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use dotenvy::dotenv;

    #[tokio::test]
    async fn chat() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let chat_completion = ChatCompletion::builder(
            "gpt-3.5-turbo",
            [ChatCompletionMessage {
                role: ChatCompletionMessageRole::User,
                content: Some("Hello!".to_string()),
                name: None,
                function_call: None,
                tool_call_id: None,
                tool_calls: Some(Vec::new()),
            }],
        )
        .temperature(0.0)
        .response_format(ChatCompletionResponseFormat::Text)
        .credentials(credentials)
        .create()
        .await
        .unwrap();

        assert_eq!(
            chat_completion
                .choices
                .first()
                .unwrap()
                .message
                .content
                .as_ref()
                .unwrap(),
            "Hello! How can I assist you today?"
        );
    }

    // Seeds are not deterministic so the only point of the test is to
    // ensure that passing a seed still results in a valid response.
    #[tokio::test]
    async fn chat_seed() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let chat_completion = ChatCompletion::builder(
            "gpt-3.5-turbo",
            [ChatCompletionMessage {
                role: ChatCompletionMessageRole::User,
                content: Some(
                    "What type of seed does Mr. England sow in the song? Reply with 1 word."
                        .to_string(),
                ),
                name: None,
                function_call: None,
                tool_call_id: None,
                tool_calls: Some(Vec::new()),
            }],
        )
        // Determinism currently comes from temperature 0, not seed.
        .temperature(0.0)
        .seed(1337u64)
        .credentials(credentials)
        .create()
        .await
        .unwrap();

        assert_eq!(
            chat_completion
                .choices
                .first()
                .unwrap()
                .message
                .content
                .as_ref()
                .unwrap(),
            "Love"
        );
    }

    #[tokio::test]
    async fn chat_stream() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let chat_stream = ChatCompletion::builder(
            "gpt-3.5-turbo",
            [ChatCompletionMessage {
                role: ChatCompletionMessageRole::User,
                content: Some("Hello!".to_string()),
                name: None,
                function_call: None,
                tool_call_id: None,
                tool_calls: Some(Vec::new()),
            }],
        )
        .temperature(0.0)
        .credentials(credentials)
        .create_stream()
        .await
        .unwrap();

        let chat_completion = stream_to_completion(chat_stream).await;

        assert_eq!(
            chat_completion
                .choices
                .first()
                .unwrap()
                .message
                .content
                .as_ref()
                .unwrap(),
            "Hello! How can I assist you today?"
        );
    }

    #[tokio::test]
    async fn chat_function() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let chat_stream = ChatCompletion::builder(
            "gpt-4o",
            [
                ChatCompletionMessage {
                    role: ChatCompletionMessageRole::User,
                    content: Some("What is the weather in Boston?".to_string()),
                    name: None,
                    function_call: None,
                    tool_call_id: None,
                    tool_calls: Some(Vec::new()),
                }
            ]
        ).functions([ChatCompletionFunctionDefinition {
            description: Some("Get the current weather in a given location.".to_string()),
            name: "get_current_weather".to_string(),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The city and state to get the weather for. (eg: San Francisco, CA)"
                    }
                },
                "required": ["location"]
            })),
        }])
        .temperature(0.2)
        .credentials(credentials)
        .create_stream()
        .await
        .unwrap();

        let chat_completion = stream_to_completion(chat_stream).await;

        assert_eq!(
            chat_completion
                .choices
                .first()
                .unwrap()
                .message
                .function_call
                .as_ref()
                .unwrap()
                .name,
            "get_current_weather".to_string(),
        );

        assert_eq!(
            serde_json::from_str::<Value>(
                &chat_completion
                    .choices
                    .first()
                    .unwrap()
                    .message
                    .function_call
                    .as_ref()
                    .unwrap()
                    .arguments
            )
            .unwrap(),
            serde_json::json!({
                "location": "Boston, MA"
            }),
        );
    }

    #[tokio::test]
    async fn chat_response_format_json() {
        dotenv().ok();
        let credentials = Credentials::from_env();
        let chat_completion = ChatCompletion::builder(
            "gpt-3.5-turbo",
            [ChatCompletionMessage {
                role: ChatCompletionMessageRole::User,
                content: Some("Write an example JSON for a JWT header using RS256".to_string()),
                name: None,
                function_call: None,
                tool_call_id: None,
                tool_calls: Some(Vec::new()),
            }],
        )
        .temperature(0.0)
        .seed(1337u64)
        .response_format(ChatCompletionResponseFormat::JsonObject)
        .credentials(credentials)
        .create()
        .await
        .unwrap();
        let response_string = chat_completion
            .choices
            .first()
            .unwrap()
            .message
            .content
            .as_ref()
            .unwrap();
        #[derive(Deserialize, Eq, PartialEq, Debug)]
        struct Response {
            alg: String,
            typ: String,
        }
        let response = serde_json::from_str::<Response>(response_string).unwrap();
        assert_eq!(
            response,
            Response {
                alg: "RS256".to_owned(),
                typ: "JWT".to_owned()
            }
        );
    }

    #[test]
    fn builder_clone_and_eq() {
        let builder_a = ChatCompletion::builder("gpt-4", [])
            .temperature(0.0)
            .seed(65u64);
        let builder_b = builder_a.clone();
        let builder_c = builder_b.clone().temperature(1.0);
        let builder_d = ChatCompletionBuilder::default();
        assert_eq!(builder_a, builder_b);
        assert_ne!(builder_a, builder_c);
        assert_ne!(builder_b, builder_c);
        assert_ne!(builder_a, builder_d);
        assert_ne!(builder_c, builder_d);
    }

    async fn stream_to_completion(
        mut chat_stream: Receiver<ChatCompletionDelta>,
    ) -> ChatCompletion {
        let mut merged: Option<ChatCompletionDelta> = None;
        while let Some(delta) = chat_stream.recv().await {
            match merged.as_mut() {
                Some(c) => {
                    c.merge(delta).unwrap();
                }
                None => merged = Some(delta),
            };
        }
        merged.unwrap().into()
    }

    #[derive(JsonSchema, Deserialize, Debug, Eq, PartialEq)]
    enum Race {
        Black,
        White,
        Asian,
        Other(String),
    }
    #[derive(JsonSchema, Deserialize, Debug, PartialEq)]
    enum Species {
        Human(Race),
        Orc { color: String, leader: String },
    }
    #[derive(JsonSchema, Deserialize, Debug, PartialEq)]
    struct Character {
        pub name: String,
        pub age: i64,
        pub power: f64,
        pub skills: Vec<Skill>,
        pub species: Species,
    }
    #[derive(JsonSchema, Deserialize, Debug, PartialEq)]
    struct Skill {
        pub name: String,
        pub description: Option<String>,
        pub dont_use_this_property: Option<String>,
    }

    #[tokio::test]
    async fn chat_structured_output_completion() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let format =
            ChatCompletionResponseFormat::json_schema::<Character>(true, JsonSchemaStyle::OpenAI);
        let chat_completion = ChatCompletion::builder(
            "gpt-4o-mini",
            [ChatCompletionMessage {
                role: ChatCompletionMessageRole::User,
                content: Some(
                    "Create a DND character, don't use the dont_use_this_property field"
                        .to_string(),
                ),
                ..Default::default()
            }],
        )
        .credentials(credentials)
        .response_format(format)
        .create()
        .await
        .unwrap();
        let character_str = chat_completion.choices[0].message.content.as_ref().unwrap();
        let _character: Character = serde_json::from_str(character_str).unwrap();
    }

    #[tokio::test]
    async fn chat_tool_use_completion() {
        dotenv().ok();
        let credentials = Credentials::from_env();
        let schema = ChatCompletionTool::new::<Character>(None);
        let chat_completion = ChatCompletion::builder(
            "gpt-4o-mini",
            [ChatCompletionMessage {
                role: ChatCompletionMessageRole::User,
                content: Some("create a random DND character directly with tools".to_string()),
                ..Default::default()
            }],
        )
        .credentials(credentials)
        .tools(vec![schema])
        .tool_choice(ToolChoice::Function {
            r#type: FunctionType::Function,
            function: FunctionChoice {
                name: "Character".to_string(),
            },
        })
        .create()
        .await
        .unwrap();
        let msg = chat_completion.choices[0].message.clone();
        let tool_calls = msg.tool_calls.as_ref();
        let tool_call: &ToolCall = tool_calls.unwrap().first().unwrap();
        let _character: Character = serde_json::from_str(&tool_call.function.arguments).unwrap();
    }

    #[tokio::test]
    async fn chat_tool_response_completion() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let chat_completion = ChatCompletion::builder(
            "gpt-4o-mini",
            [
                ChatCompletionMessage {
                    role: ChatCompletionMessageRole::User,
                    content: Some(
                        "What's 0.9102847*28456? \
                        reply in plain text, \
                        round the number to to 2 decimals \
                        and reply with the result number only, \
                        with no full stop at the end"
                            .to_string(),
                    ),
                    name: None,
                    function_call: None,
                    tool_call_id: None,
                    tool_calls: Some(Vec::new()),
                },
                ChatCompletionMessage {
                    role: ChatCompletionMessageRole::Assistant,
                    content: Some("Let me calculate that for you.".to_string()),
                    name: None,
                    function_call: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "the_tool_call".to_string(),
                        r#type: FunctionType::Function,
                        function: ToolCallFunction {
                            name: "mul".to_string(),
                            arguments: "not_required_to_be_valid_here".to_string(),
                        },
                    }]),
                },
                ChatCompletionMessage {
                    role: ChatCompletionMessageRole::Tool,
                    content: Some("the result is 25903.061423199997".to_string()),
                    name: None,
                    function_call: None,
                    tool_call_id: Some("the_tool_call".to_string()),
                    tool_calls: Some(Vec::new()),
                },
            ],
        )
        // Determinism currently comes from temperature 0, not seed.
        .temperature(0.0)
        .seed(1337u64)
        .credentials(credentials)
        .create()
        .await
        .unwrap();

        assert_eq!(
            chat_completion
                .choices
                .first()
                .unwrap()
                .message
                .content
                .as_ref()
                .unwrap(),
            "25903.06"
        );
    }
}

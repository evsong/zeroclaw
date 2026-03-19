## ADDED Requirements

### Requirement: ChannelMessage supports image URLs
The `ChannelMessage` struct SHALL include an `image_urls: Option<Vec<String>>` field that allows channels to attach image URLs to incoming messages. When no images are present, this field MUST be `None`. All existing channel implementations MUST set this field to `None` to maintain backwards compatibility.

#### Scenario: Text-only message has no image URLs
- **WHEN** a channel produces a `ChannelMessage` with only text content
- **THEN** the `image_urls` field MUST be `None`

#### Scenario: Message with image attachments carries URLs
- **WHEN** a channel produces a `ChannelMessage` with image attachments
- **THEN** the `image_urls` field MUST be `Some(vec![...])` containing one URL per image

### Requirement: ChatMessage supports image URLs
The `ChatMessage` struct SHALL include an `image_urls: Option<Vec<String>>` field. Existing constructors (`system`, `user`, `assistant`, `tool`) MUST set this field to `None`. A new `user_with_images` constructor MUST accept a `Vec<String>` of image URLs and set the field accordingly (empty vec → `None`).

#### Scenario: Standard user message has no images
- **WHEN** `ChatMessage::user("hello")` is called
- **THEN** the resulting message MUST have `image_urls: None`

#### Scenario: Multimodal user message carries image URLs
- **WHEN** `ChatMessage::user_with_images("describe this", vec!["https://cdn.discord.com/img.png"])` is called
- **THEN** the resulting message MUST have `image_urls: Some(vec!["https://cdn.discord.com/img.png"])`

#### Scenario: Empty image URL list becomes None
- **WHEN** `ChatMessage::user_with_images("hello", vec![])` is called
- **THEN** the resulting message MUST have `image_urls: None`

### Requirement: ChatMessage serialization is backwards compatible
The `image_urls` field MUST use `serde(skip_serializing_if = "Option::is_none", default)` so that: (a) text-only messages serialize without an `image_urls` key, and (b) existing serialized conversation history (without `image_urls`) deserializes correctly with `image_urls: None`.

#### Scenario: Text-only ChatMessage serialization unchanged
- **WHEN** a `ChatMessage` with `image_urls: None` is serialized to JSON
- **THEN** the output MUST NOT contain an `image_urls` key

#### Scenario: Legacy conversation history deserializes correctly
- **WHEN** a JSON string `{"role":"user","content":"hello"}` (no `image_urls` key) is deserialized into `ChatMessage`
- **THEN** the resulting struct MUST have `image_urls: None`

### Requirement: Agent pipeline threads image URLs from channel to provider
When `process_channel_message` receives a `ChannelMessage` with `image_urls` set, it MUST create a `ChatMessage` using `user_with_images` so that image URLs are preserved in the conversation history and passed to the provider.

#### Scenario: Image URLs flow from ChannelMessage to ChatMessage
- **WHEN** a `ChannelMessage` arrives with `image_urls: Some(vec!["https://example.com/photo.jpg"])`
- **THEN** the `ChatMessage` appended to conversation history MUST have `image_urls: Some(vec!["https://example.com/photo.jpg"])`

#### Scenario: ChannelMessage without images creates standard ChatMessage
- **WHEN** a `ChannelMessage` arrives with `image_urls: None`
- **THEN** the `ChatMessage` appended to conversation history MUST have `image_urls: None`

### Requirement: OpenAI provider sends multimodal content array for messages with images
When a `ChatMessage` with `image_urls` is sent to the OpenAI provider, the provider MUST serialize the `content` field as an array of content parts (OpenAI vision format) instead of a plain string. The array MUST contain one `text` part followed by one `image_url` part per URL.

#### Scenario: Message with images produces multimodal content array
- **WHEN** a `ChatMessage` with `content: "what is this?"` and `image_urls: Some(vec!["https://cdn.discord.com/img.png"])` is converted to a `NativeMessage`
- **THEN** the `content` field MUST be a JSON array: `[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"https://cdn.discord.com/img.png"}}]`

#### Scenario: Message without images produces plain string content
- **WHEN** a `ChatMessage` with `content: "hello"` and `image_urls: None` is converted to a `NativeMessage`
- **THEN** the `content` field MUST be a JSON string: `"hello"`

#### Scenario: Multiple images produce multiple image_url parts
- **WHEN** a `ChatMessage` has `image_urls: Some(vec!["url1.png", "url2.png"])`
- **THEN** the `content` array MUST contain one `text` part and two `image_url` parts in order

### Requirement: NativeMessage content field supports both string and array
The OpenAI provider's internal `NativeMessage.content` field MUST be `Option<serde_json::Value>` to support both plain string (`Value::String`) and multimodal array (`Value::Array`) serialization. Text-only messages MUST use `Value::String` to maintain wire-format compatibility with existing providers.

#### Scenario: Text-only NativeMessage wire format unchanged
- **WHEN** a text-only `NativeMessage` is serialized
- **THEN** the JSON output for `content` MUST be a string (e.g., `"content": "hello"`), not an array

#### Scenario: Multimodal NativeMessage uses array format
- **WHEN** a multimodal `NativeMessage` is serialized
- **THEN** the JSON output for `content` MUST be an array of content parts

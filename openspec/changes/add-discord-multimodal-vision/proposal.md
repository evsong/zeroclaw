## Why

Discord users send image attachments alongside text messages (e.g., "装修一下这个空间" + a photo), but ZeroClaw's Discord channel currently only processes `text/*` MIME type attachments—image attachments are silently skipped by `process_attachments()`. This means the LLM never sees the user's images, even when the upstream provider (GPT-5.4, Claude) fully supports vision/multimodal input. This blocks the entire image-to-image generation workflow (ComfyUI img2img) and any vision-based use case (image analysis, OCR, visual Q&A).

## What Changes

- **Extract image URLs from Discord attachments**: Instead of skipping `image/*` attachments, collect their CDN URLs for downstream multimodal processing.
- **Thread image URLs through the message pipeline**: Add an `image_urls` field to `ChannelMessage` and `ChatMessage` so image context flows from channel → agent → provider without loss.
- **Send multimodal content to OpenAI-compatible providers**: When a `ChatMessage` has `image_urls`, the OpenAI provider serializes `content` as an array of `text` + `image_url` content parts (OpenAI vision format) instead of a plain string.
- **Backwards compatible**: All existing text-only messages are unaffected. Channels that don't set `image_urls` (Telegram, Slack, CLI, etc.) work identically to before. The `image_urls` field is `Option<Vec<String>>` with default `None`.

## Capabilities

### New Capabilities
- `multimodal-vision`: Core support for threading image URLs from channel messages through to LLM providers as `image_url` content parts. Covers the `ChannelMessage`, `ChatMessage`, and provider serialization changes.
- `discord-image-attachments`: Discord-specific extraction of `image/*` attachment URLs and population of `ChannelMessage.image_urls`.

### Modified Capabilities
<!-- No existing spec-level requirements change. This is additive. -->

## Impact

- **`src/channels/traits.rs`**: `ChannelMessage` struct gains `image_urls: Option<Vec<String>>` field. All existing channel implementations must include this field (set to `None`).
- **`src/providers/traits.rs`**: `ChatMessage` struct gains `image_urls: Option<Vec<String>>` field. All constructors (`system`, `user`, `assistant`, `tool`) set it to `None`. New `user_with_images` constructor added.
- **`src/providers/openai.rs`**: `NativeMessage.content` changes from `Option<String>` to `Option<serde_json::Value>` to support both string and array content. `convert_messages` builds multimodal content arrays when `image_urls` is present.
- **`src/channels/discord.rs`**: New `process_attachments_multimodal` function replaces `process_attachments` at the call site. Image URLs are collected and passed through `ChannelMessage`.
- **`src/channels/mod.rs`**: `process_channel_message` uses `ChatMessage::user_with_images` when `msg.image_urls` is set.
- **All other channel implementations** (telegram, slack, cli, lark, irc, matrix, etc.): Must add `image_urls: None` to their `ChannelMessage` construction. No behavior change.
- **All test files**: `ChannelMessage` literals in tests must include `image_urls: None`.
- **No breaking API changes**: Wire format for text-only messages is identical. Only messages with images get the array content format.
- **Dependencies**: No new crate dependencies required.

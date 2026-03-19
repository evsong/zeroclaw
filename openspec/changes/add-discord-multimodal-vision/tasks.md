## 1. Core Struct Changes

- [x] 1.1 Add `image_urls: Option<Vec<String>>` field to `ChannelMessage` in `src/channels/traits.rs`
- [x] 1.2 Add `image_urls: Option<Vec<String>>` field to `ChatMessage` in `src/providers/traits.rs` with `#[serde(skip_serializing_if = "Option::is_none", default)]`
- [x] 1.3 Update all `ChatMessage` constructors (`system`, `user`, `assistant`, `tool`) to set `image_urls: None`
- [x] 1.4 Add `ChatMessage::user_with_images(content, image_urls)` constructor that sets `image_urls` to `Some(urls)` when non-empty, `None` when empty

## 2. OpenAI Provider Multimodal Support

- [x] 2.1 Change `NativeMessage.content` from `Option<String>` to `Option<serde_json::Value>` in `src/providers/openai.rs`
- [x] 2.2 Add `build_multimodal_content(text, image_urls) -> serde_json::Value` helper function that builds the OpenAI vision content array
- [x] 2.3 Update `convert_messages()` to use `Value::String` for text-only messages and `build_multimodal_content` for messages with `image_urls`
- [x] 2.4 Update assistant tool-call message content conversion to use `Value::String` instead of `String`
- [x] 2.5 Update tool result message content conversion to use `Value::String` instead of `String`

## 3. Discord Image Attachment Extraction

- [x] 3.1 Add `process_attachments_multimodal()` function in `src/channels/discord.rs` returning `(String, Vec<String>)` — text parts and image URLs
- [x] 3.2 Handle `image/*` MIME types by collecting URLs into the image list with info-level logging
- [x] 3.3 Preserve existing `text/*` fetch-and-inline behavior in the new function
- [x] 3.4 Skip other MIME types with debug log (matching existing behavior)
- [x] 3.5 Replace `process_attachments()` call site in `MESSAGE_CREATE` handler with `process_attachments_multimodal()`
- [x] 3.6 Populate `ChannelMessage.image_urls` from extracted image URLs (None if empty)

## 4. Agent Pipeline Threading

- [x] 4.1 Update `process_channel_message()` in `src/channels/mod.rs` to use `ChatMessage::user_with_images` when `msg.image_urls` is set
- [x] 4.2 Verify image URLs are preserved through `append_sender_turn` → conversation history → provider call chain

## 5. Backfill `image_urls: None` Across All Channels

- [x] 5.1 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/cli.rs`
- [x] 5.2 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/telegram.rs`
- [x] 5.3 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/slack.rs`
- [x] 5.4 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/lark.rs`
- [x] 5.5 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/irc.rs`
- [x] 5.6 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/matrix.rs`
- [x] 5.7 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/nostr.rs`
- [x] 5.8 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/mattermost.rs`
- [x] 5.9 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/wati.rs`
- [x] 5.10 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/whatsapp.rs`
- [x] 5.11 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/whatsapp_web.rs`
- [x] 5.12 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/email_channel.rs`
- [x] 5.13 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/imessage.rs`
- [x] 5.14 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/signal.rs`
- [x] 5.15 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/dingtalk.rs`
- [x] 5.16 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/qq.rs`
- [x] 5.17 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/linq.rs`
- [x] 5.18 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/nextcloud_talk.rs`
- [x] 5.19 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/traits.rs` (webhook/test helpers)
- [x] 5.20 Add `image_urls: None` to `ChannelMessage` construction in `src/channels/mod.rs` (all test fixtures)
- [x] 5.21 Add `image_urls: None` to `ChannelMessage` construction in `src/gateway/mod.rs`
- [x] 5.22 Add `image_urls: None` to `ChannelMessage` construction in `src/observability/traits.rs`
- [x] 5.23 Add `image_urls: None` to Discord `INTERACTION_CREATE` handler ChannelMessage in `src/channels/discord.rs`

## 6. Fix Existing Test Compilation

- [x] 6.1 Add `image_urls: None` to all `NativeMessage` test literals in `src/providers/openai.rs` (update content field to `serde_json::Value`)
- [x] 6.2 Update `convert_messages` test assertions for the new `Value::String` content type
- [x] 6.3 Ensure existing `process_attachments` tests in `src/channels/discord.rs` still pass unchanged

## 7. New Tests

- [x] 7.1 Add unit test: `process_attachments_multimodal` collects image URLs
- [x] 7.2 Add unit test: `process_attachments_multimodal` still inlines text attachments
- [x] 7.3 Add unit test: `process_attachments_multimodal` skips unsupported types
- [x] 7.4 Add unit test: `ChatMessage::user_with_images` sets image_urls correctly
- [x] 7.5 Add unit test: `ChatMessage::user_with_images` with empty vec returns None
- [x] 7.6 Add unit test: `ChatMessage` serde round-trip preserves image_urls
- [x] 7.7 Add unit test: `ChatMessage` serde round-trip with None omits field
- [x] 7.8 Add unit test: `build_multimodal_content` produces correct OpenAI format
- [x] 7.9 Add unit test: `convert_messages` with image_urls produces array content
- [x] 7.10 Add unit test: `convert_messages` without image_urls produces string content

## 8. Build & Deploy

- [x] 8.1 Run `cargo check` — verify zero compilation errors
- [x] 8.2 Run `cargo clippy --all-targets -- -D warnings` — no new warnings
- [x] 8.3 Run `cargo test` — all tests pass (existing + new)
- [x] 8.4 Cross-compile: `cargo zigbuild --release --target x86_64-unknown-linux-gnu --features channel-lark`
- [x] 8.5 Deploy to lt-server: gzip + scp + systemctl restart zeroclaw
- [ ] 8.6 Verify: send image + text in Discord, confirm LLM response references image content
- [ ] 8.7 Check logs for `collected image attachment for vision` info line

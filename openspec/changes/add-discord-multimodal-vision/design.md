## Context

ZeroClaw is a Rust AI agent runtime with a trait-driven modular architecture. Messages flow through a well-defined pipeline:

```
Channel (Discord/Telegram/...) → ChannelMessage → process_channel_message() → ChatMessage → Provider (OpenAI/...) → LLM API
```

**Current state**: `ChannelMessage` and `ChatMessage` are text-only structs (`content: String`). The Discord channel's `process_attachments()` function explicitly filters for `text/*` MIME types and silently drops all other attachment types including images. The OpenAI provider serializes `content` as a plain JSON string.

**Desired state**: When a Discord user sends an image attachment, the image URL flows through the entire pipeline and reaches the LLM as an OpenAI-compatible `image_url` content part, enabling vision capabilities.

**Constraints**:
- ZeroClaw has ~20+ channel implementations and ~60+ `ChannelMessage` construction sites across the codebase
- The change must be backwards-compatible: text-only messages must serialize identically to before
- `ChatMessage` is serialized/deserialized for SQLite conversation history persistence
- The OpenAI provider serves as the base for all custom API providers (CLIProxyAPI, etc.)

## Goals / Non-Goals

**Goals:**
- Discord image attachments are passed to the LLM as `image_url` content parts
- GPT-5.4 and other vision-capable models can "see" images sent in Discord
- Text-only messages across all channels remain completely unaffected
- Conversation history persistence (SQLite) handles multimodal messages correctly
- Clean, minimal diff that follows ZeroClaw's trait-driven architecture

**Non-Goals:**
- Supporting image attachments from other channels (Telegram, Slack, etc.) — future work, but the core structs are ready for it
- Image content embedding (base64 inline) — we pass URLs only, the LLM fetches them
- Multimodal output (LLM returning images) — ZeroClaw already handles this via `[IMAGE:]` markers
- Supporting video or audio attachments — out of scope
- Anthropic provider multimodal support — only OpenAI format is needed (CLIProxyAPI uses OpenAI format)

## Decisions

### Decision 1: `Option<Vec<String>>` field vs content union type

**Chosen**: Add `image_urls: Option<Vec<String>>` to both `ChannelMessage` and `ChatMessage`.

**Alternative considered**: Change `content` to an enum like `Content::Text(String) | Content::Multimodal(Vec<ContentPart>)`. This would be more "correct" but requires changing every single content access site across the entire codebase (hundreds of locations). The `Option` field approach is additive — existing code that reads `content` as text continues to work unchanged.

**Rationale**: Minimal diff, backwards-compatible serialization (serde skips `None` fields), and matches how providers like Anthropic model multimodal messages (separate `images` array alongside `content`).

### Decision 2: `NativeMessage.content` as `serde_json::Value`

**Chosen**: Change the OpenAI provider's internal `NativeMessage.content` from `Option<String>` to `Option<serde_json::Value>`.

**Rationale**: OpenAI's API accepts `content` as either a string or an array of content parts. Using `serde_json::Value` handles both cases without needing a custom enum + serializer. When there are no images, we emit `Value::String(text)` (identical wire format to before). When there are images, we emit `Value::Array([{type: "text", ...}, {type: "image_url", ...}])`.

**Alternative considered**: Custom `#[serde(untagged)]` enum. More type-safe but adds complexity for a simple two-variant case that `serde_json::Value` handles trivially.

### Decision 3: New function `process_attachments_multimodal` instead of modifying `process_attachments`

**Chosen**: Create a new function that returns `(String, Vec<String>)` — text parts and image URLs separately.

**Rationale**: The original `process_attachments` is covered by unit tests and used as the text-only path. Keeping it intact preserves test validity. The new function has a clear multimodal-specific name. The old function can be deprecated later.

### Decision 4: Image URLs passed by reference, not fetched/embedded

**Chosen**: Pass Discord CDN URLs directly to the LLM provider. The LLM fetches the images itself.

**Rationale**: Discord CDN URLs are publicly accessible (no auth needed). Fetching and base64-encoding images would increase payload size 4x and add latency. OpenAI/GPT-5.4 natively supports `image_url` type content parts with direct URLs.

**Risk**: Discord CDN URLs have expiration (typically hours). For conversation history replay, old URLs may be expired. Acceptable trade-off — real-time vision is the primary use case.

### Decision 5: All non-Discord channels set `image_urls: None`

**Chosen**: Every `ChannelMessage` construction site outside Discord must add `image_urls: None`.

**Rationale**: Rust requires all struct fields to be initialized. There's no default/builder pattern on `ChannelMessage` currently. Adding `#[derive(Default)]` would require all fields to implement `Default` (including `String` fields that should not be empty). The explicit `None` approach is verbose but safe and compiler-verified.

**Future**: A builder pattern or `..Default::default()` could reduce boilerplate, but that's a separate refactor.

## Risks / Trade-offs

**[Risk] Compilation breakage across 60+ construction sites** → Mitigation: `cargo check` will report every missing `image_urls` field. Fix mechanically by adding `image_urls: None` to each. No logic changes needed at these sites.

**[Risk] Discord CDN URL expiration in conversation history** → Mitigation: Acceptable for now. The primary use case is real-time vision (current turn). Historical turns with expired image URLs will have broken images but text context is preserved.

**[Risk] `serde_json::Value` for content loses type safety** → Mitigation: Only used in the OpenAI provider's internal `NativeMessage` struct, not in the public API. The `build_multimodal_content` helper function encapsulates the construction logic.

**[Risk] Large images consuming LLM context/tokens** → Mitigation: OpenAI automatically resizes images. We could add a `max_image_attachments` config option later, but for now the Discord limit of 10 attachments per message is sufficient.

**[Risk] SQLite serialization of `image_urls`** → Mitigation: `ChatMessage` uses `serde(skip_serializing_if = "Option::is_none", default)` for `image_urls`. Existing serialized history (without this field) deserializes correctly with `default` (→ `None`). New history with images serializes the URLs as JSON array.

## Migration Plan

1. **Compile & test locally**: `cargo check`, then `cargo test` to verify no regressions
2. **Cross-compile**: `cargo zigbuild --release --target x86_64-unknown-linux-gnu --features channel-lark`
3. **Deploy**: gzip + scp to lt-server, `systemctl stop zeroclaw`, replace binary, `systemctl start zeroclaw`
4. **Verify**: Send image + text in Discord, check `journalctl -u zeroclaw` for `collected image attachment for vision` log line, verify LLM response references the image content
5. **Rollback**: Revert to previous binary (`/opt/zeroclaw/zeroclaw.bak`)

## Open Questions

- Should we add `image_urls` support to the Lark/飞书 channel as well? It has similar attachment handling. → Defer to follow-up change.
- Should there be a config option to disable multimodal? (e.g., for providers that don't support vision) → Not needed now; providers that don't understand `image_url` content parts will ignore them or error, which is acceptable.

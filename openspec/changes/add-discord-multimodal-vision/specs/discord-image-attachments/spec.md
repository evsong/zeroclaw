## ADDED Requirements

### Requirement: Discord channel extracts image attachment URLs
The Discord channel MUST implement a `process_attachments_multimodal` function that processes Discord message attachments and returns both inlined text content (for `text/*` MIME types) and a list of image URLs (for `image/*` MIME types). All other MIME types MUST be silently skipped with a debug log.

#### Scenario: Image attachment URL is collected
- **WHEN** a Discord message contains an attachment with `content_type: "image/png"` and `url: "https://cdn.discordapp.com/attachments/123/456/photo.png"`
- **THEN** the URL MUST be included in the returned image URL list

#### Scenario: Multiple image attachments are all collected
- **WHEN** a Discord message contains 3 image attachments
- **THEN** all 3 URLs MUST be included in the returned image URL list in order

#### Scenario: Text attachment is still inlined as text
- **WHEN** a Discord message contains an attachment with `content_type: "text/plain"`
- **THEN** the text content MUST be fetched and inlined in the returned text string (existing behavior preserved)

#### Scenario: Non-image non-text attachment is skipped
- **WHEN** a Discord message contains an attachment with `content_type: "application/pdf"`
- **THEN** the attachment MUST be skipped with a debug log and not included in either return value

#### Scenario: Attachment without URL is skipped
- **WHEN** a Discord message contains an attachment without a `url` field
- **THEN** the attachment MUST be skipped with a warning log

### Requirement: Discord ChannelMessage includes image URLs
When constructing a `ChannelMessage` from a Discord `MESSAGE_CREATE` event, the Discord channel MUST populate the `image_urls` field with the image URLs extracted by `process_attachments_multimodal`. If no images are found, `image_urls` MUST be `None`.

#### Scenario: Discord message with image produces ChannelMessage with image_urls
- **WHEN** a Discord `MESSAGE_CREATE` event contains a text message "describe this" and one image attachment
- **THEN** the resulting `ChannelMessage` MUST have `content` containing the text and `image_urls: Some(vec![...])` with the image CDN URL

#### Scenario: Discord message without images has image_urls None
- **WHEN** a Discord `MESSAGE_CREATE` event contains only text
- **THEN** the resulting `ChannelMessage` MUST have `image_urls: None`

### Requirement: Image attachment collection is logged
When an image attachment URL is collected, the Discord channel MUST emit an `info`-level log containing the filename and URL for observability.

#### Scenario: Image collection is logged
- **WHEN** an image attachment with filename "photo.png" is processed
- **THEN** an info log MUST be emitted containing "collected image attachment for vision" and the filename

### Requirement: Original process_attachments function is preserved
The original `process_attachments` function MUST remain in the codebase unchanged for backwards compatibility and existing test coverage. The new `process_attachments_multimodal` function MUST be used at the Discord `MESSAGE_CREATE` handler call site instead.

#### Scenario: Old function still compiles and passes tests
- **WHEN** `cargo test` is run
- **THEN** all existing `process_attachments` unit tests MUST pass without modification

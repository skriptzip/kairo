# Local patch

This is `outlook-pst` 1.2.0 (MIT, Microsoft) with one change, so that Kairo can import calendars
that contain simple appointments:

`src/messaging/message.rs`, `MessageInner::read_embedded`: a message without a sub-node tree
(no recipients and no attachments, e.g. public holiday entries) is accepted instead of failing
with `Missing Sub-Node Tree on message`. Examples and dev-dependencies were dropped.

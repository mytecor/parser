# External Playwright Adapter Contract

The adapter and the JS/Rust bridge are separate components. Parser Core does not
perform browser actions or store Locator/ElementHandle/backendNodeId values.

1. Build the HTML and SourceMap during the same DOM serialization pass. Measure
   positions in UTF-8 bytes of the actual output, including escaped attributes
   and text. `page.content()` alone is insufficient without a map.
2. Assign an opaque, unique snapshot ID and bind it to the page, frame,
   navigation epoch, and DOM version. Store the SHA-256 hash of the exact HTML bytes.
3. Keep live DOM references for element ranges inside the adapter only. Do not
   modify the DOM to add internal IDs. Create a separate snapshot for each
   iframe and account for whether its frame is accessible.
4. Send the HTML to the Rust host with `snapshot_id`. Retain the returned
   `snapshot_id`, `content_hash`, ranges, and rule revision.
5. Resolve a range through the map only when the snapshot ID, hash, page, frame,
   and epoch match. Require an exact range match for an element. For captures
   of text or nested nodes, use an explicitly defined policy for selecting the
   containing element; reject ambiguous results.
6. Before acting, verify the DOM version, the element's connected state, and its
   membership in the frame. Tie validation and use to a version token; a DOM
   change between them invalidates the result. If this relationship cannot be
   guaranteed, cancel the action and obtain a fresh snapshot.
7. If the snapshot is stale, run snapshot → parse → resolve again. Never apply
   an old range to new HTML, even if the element text is identical.
8. Obtain a new snapshot after an action. Do not automatically retry an action
   with side effects if it is unclear whether the action already occurred.

Convert JavaScript UTF-16 indices to UTF-8 only at the adapter boundary. Do not
use JavaScript string length as a byte count. For example, `🌏` occupies two
UTF-16 code units and four UTF-8 bytes. Detached elements, shadow roots, dynamic
form properties, and iframe serialization require explicit adapter policies:
the map describes only the snapshot that was actually serialized.

Adapter acceptance check: snapshot A → mutation/navigation → an attempt to
resolve a range from A must fail before any action. This is a requirement for
the future adapter; the Rust library returns the snapshot ID and hash, while
the calling application checks whether the live DOM is still current.

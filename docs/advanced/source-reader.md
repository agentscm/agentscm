# Reusable immutable source reads

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

`store::SourceReader` reads native source objects through an
`EncodedObjectReader` supplied by a controller. It shares the production codec,
manifest traversal and diff implementation with the local `Engine`. It does
not require a temporary store or a copy of the repository. The `SourceRead`
trait also lets history and relation pagination use either reader.

Each requested object ID is checked before the callback opens it. The reader
consumes at most `MAX_OBJECT_BYTES + 1` bytes, rejects an oversized object, and
checks its digest and native encoding before using it. High-level operations
still enforce their existing file, page and traversal limits. A callback must
bound its own buffering, blocking time and backend work; this synchronous API
does not add a deadline or cancellation mechanism. Cache buffers, parsing and
decoded file copies also consume memory. The encoded-object limit alone does
not establish a controller's total memory budget.

Unpaged diff requires strictly increasing native trie depths while descending;
it cannot recurse beyond the 64 SHA-256 trie positions. Complete qualification
shares a limit of 1,000,000 work steps across graph/index visits and derived
directory-prefix candidates. Expected distinct prefixes cannot outnumber the
actual prefix index. These limits also apply to local reads and sync and can
reject an otherwise well-formed closure with excessive qualification work.

Callbacks return `MissingObject` only when the requested canonical ID is known
to be missing. A different ID is invalid. `SourceDenied` and
`SourceIndeterminate` preserve denied and uncertain outcomes. Generic callback
errors, including a textual `NotFound`, become indeterminate. Stream permission
errors become denied and other stream errors become indeterminate; partially
read bytes are discarded. History, relation and diff pages may report typed
missing IDs with a resumable cursor. Denial and uncertainty fail the operation
instead of producing a successful partial page.

`sync::qualify_reader` validates the actual complete closure, including source
types, indexes, revision history and root antichain. It derives the source
profile and object descriptors from those bytes. Its `QualifiedSource` result
is a point-in-time source proof, not a publication permission or retention
lease. A controller must authorize every read, including cache hits, bind the
repository and manifest, compare the actual profile with its namespace, and
recheck current authorization and retention state at its transaction boundary.
Local cursor checksums do not authenticate a network request.

The reusable reader does not itself qualify authenticated sessions, uploads,
garbage collection or recovery. Those facilities need their own integrated
authorization, retention and failure tests. The independent protocol verifier
also remains a separate implementation; sharing this codec does not satisfy
that requirement.

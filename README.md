# nano-db

A single Rust HTTP/1.1 service for UUID-addressed JSON blobs, built with
Axum/Tokio and embedded redb storage. This is the core increment, not a
production-readiness or performance certification.

## Run

Install Rust 1.89 or newer with Cargo, rustfmt, and Clippy. Run from this folder:

```sh
umask 077
printf 'NANO_DB_AUTH_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
chmod 600 .env
```

Create `.env` once, then load it whenever you start the service:

```sh
set -a
. ./.env
set +a
cargo run --release
```

The `.env` file is ignored by Git. Keep it private and do not commit or share
it. The service reads environment variables from its process; source `.env` as
shown above whenever starting the service in a new shell. The service listens
at `127.0.0.1:7070` and opens `./nano-db.redb`. Its parent directory must
already exist. Only one service process may open a database file for writing;
startup fails if the file is locked, invalid, or inaccessible. On Unix, newly
created database files have mode `0600`; existing file permissions are not
changed.

For explicitly unauthenticated local development only:

```sh
NANO_DB_ALLOW_UNAUTHENTICATED=true cargo run --release
```

Unauthenticated non-loopback binding is always rejected. For remote access,
configure a token and put the service behind a trusted TLS-terminating reverse
proxy; the service itself does not implement TLS, CORS, per-user permissions,
or multi-tenancy. Never send bearer tokens over an untrusted plaintext network.

Logs are structured JSON on stderr. `RUST_LOG` controls filtering, defaulting
to `nano_db=info`. Tokens and document contents are not logged.
SIGINT/Ctrl-C and SIGTERM stop accepting connections and drain accepted work.

## HTTP contract

All routes require `Authorization: Bearer <token>` unless the explicit
loopback development mode is enabled. Bearer scheme matching is
case-insensitive. Tokens are compared using constant-time SHA-256 digest
comparison. There is no token-management endpoint.

| Request | Success | Meaning |
| --- | --- | --- |
| `POST /v1/objects` | `201 Created` | Generate a UUIDv7 and store the body. Return `Location` and metadata headers, with an empty body. |
| `PUT /v1/objects/{guid}` | `201 Created` / `204 No Content` | Create at a caller-provided UUID or fully replace the existing value. Creation returns `Location`; both return the new ETag and metadata. |
| `GET /v1/objects/{guid}` | `200 OK` / `304 Not Modified` | Return the exact stored JSON bytes, or no body for a matching cache validator. |
| `HEAD /v1/objects/{guid}` | `200 OK` / `304 Not Modified` | Return the same representation headers as GET, without loading/copying the document payload into the response. No response body. |
| `DELETE /v1/objects/{guid}` | `204 No Content` | Atomically remove document and metadata. An unconditional delete of an absent object also returns 204. |

`Location` is an origin-relative canonical lowercase, hyphenated UUID path.
Caller-provided IDs accept UUID syntax supported by Rust's `uuid` parser,
including hyphenated, simple hexadecimal, and braced forms. UUID versions
are not restricted, and nil UUIDs are valid. IDs identify resources, not fields
inside the document.

JSON bodies must use `Content-Type: application/json`, optionally with
`charset=utf-8`. Missing content types, non-JSON media types (including vendor
`+json` types), non-UTF-8 charsets, and encodings other than `identity` return
415. Partial writes using `Content-Range` return 400. There is no PATCH/merge.

Any JSON value is valid, including scalar values and arrays. Whitespace, key
order, numeric spelling, and all original valid UTF-8 bytes are preserved:
documents are never parsed into floating-point values or reserialized.
Number validation checks JSON grammar, not representability in `f64` or
`u64`; numbers such as `9007199254740993` and `1e999999` round-trip unchanged.
Malformed JSON, trailing content, non-finite numeric literals, invalid UTF-8,
unpaired Unicode surrogates, and duplicate keys in the same object return 400.
Duplicate detection compares decoded key strings, so `"a"` and `"\u0061"`
are duplicates; it does not perform Unicode normalization.

The body contains data only. Query parameters are reserved for future
metadata/functions: **all query strings are rejected with 400 in this
increment**, rather than silently applying or ignoring unsupported options.
Missing objects return 404 on GET/HEAD; unsupported methods return 405 with
`Allow`. Paths are exact; trailing slashes do not redirect.

### Metadata and conditional requests

GET/HEAD return `Content-Type: application/json`, `Content-Length` (stored byte
count, including whitespace), `ETag`, and:

| Header | Value |
| --- | --- |
| `X-Nano-Revision` | Global, monotonically increasing, persisted unsigned 64-bit document-write revision |
| `X-Nano-Size-Bytes` | Stored document byte count, including whitespace |
| `X-Nano-Created-At-Ms` | Creation time, Unix epoch milliseconds |
| `X-Nano-Updated-At-Ms` | Last replacement time, Unix epoch milliseconds |

POST/PUT return ETag and these four metadata headers, but no document body.
Replacement preserves creation time; delete/recreate gets new creation time.
Revision ordering is authoritative; timestamps depend on the system clock,
and replacement timestamps do not move backward relative to the object.

ETags are strong, opaque, quoted tokens incorporating a persisted random
database identity and global revision. Every successful document write
advances the revision, even if the bytes are unchanged. Deletion does not
rewind/reuse it, so stale validators cannot match a recreated object. A newly
initialized, different database also has different validators. Treat the ETag
as opaque, not as an ID or timestamp.

| Precondition | GET / HEAD | PUT / DELETE |
| --- | --- | --- |
| `If-Match: "<etag>"` or a list | Strong comparison; mismatch returns 412 | Strong comparison, atomically inside the write transaction; mismatch returns 412 |
| `If-Match: *` | Resource must exist (a missing GET/HEAD remains 404) | Resource must exist, otherwise 412 |
| `If-None-Match: "<etag>"` or a list | Weak comparison; match returns 304 | Weak comparison; match returns 412 |
| `If-None-Match: *` | Existing resource returns 304; absent resource returns 404 | Create-only PUT; DELETE succeeds only when already absent |

If both are present, `If-Match` is evaluated first. Weak tags cannot satisfy
`If-Match`, but can match `If-None-Match`. Lists handle quoted commas, multiple
header fields, and reasonable empty list members. Each conditional field is
limited to 8 KiB combined and 64 entity tags (plus at most 64 empty members);
malformed or excessive lists return 400. POST targets the collection, so
ETag preconditions on POST return 400; use conditional PUT with your own UUID.

304 includes ETag, metadata, and cache policy, but no body or Content-Length;
`X-Nano-Size-Bytes` still identifies the full representation size. 204 includes
neither a body nor Content-Length or Transfer-Encoding. GET/HEAD use
`Cache-Control: no-cache`; errors and
POST/PUT responses use `no-store`.

Only ETag-based preconditions are supported in this increment:
`If-Modified-Since` and `If-Unmodified-Since` return 400, and no `Last-Modified`
header is emitted. Range requests are not implemented; GET returns the
complete representation rather than 206.

### Examples

```sh
base=http://127.0.0.1:7070
id=019a1234-5678-7000-8000-123456789abc

curl -i -X POST "$base/v1/objects" \
  -H "Authorization: Bearer $NANO_DB_AUTH_TOKEN" \
  -H 'Content-Type: application/json' \
  --data-binary '{"count":9007199254740993}'

curl -i -X PUT "$base/v1/objects/$id" \
  -H "Authorization: Bearer $NANO_DB_AUTH_TOKEN" \
  -H 'Content-Type: application/json' -H 'If-None-Match: *' \
  --data-binary '{"hello":"world"}'

curl -i "$base/v1/objects/$id" \
  -H "Authorization: Bearer $NANO_DB_AUTH_TOKEN"
curl -I "$base/v1/objects/$id" \
  -H "Authorization: Bearer $NANO_DB_AUTH_TOKEN"

# Replace <etag> with the complete quoted ETag from a prior response.
curl -i -X PUT "$base/v1/objects/$id" \
  -H "Authorization: Bearer $NANO_DB_AUTH_TOKEN" \
  -H 'Content-Type: application/json' -H 'If-Match: "<etag>"' \
  --data-binary '{"hello":"replacement"}'

curl -i -X DELETE "$base/v1/objects/$id" \
  -H "Authorization: Bearer $NANO_DB_AUTH_TOKEN"
```

### Errors and retries

Application errors use `Content-Type: application/json` with this shape
(HEAD has no body):

```json
{"error":{"code":"precondition_failed","message":"the ETag precondition did not match"}}
```

| Status | Codes / cause |
| --- | --- |
| 400 | `invalid_uuid`, `invalid_json`, `invalid_body`, `invalid_precondition`, `unsupported_precondition`, `unsupported_query`, `unsupported_partial_write` |
| 401 | `unauthorized`, with `WWW-Authenticate: Bearer realm="nano-db"` |
| 404 / 405 | `not_found` / `method_not_allowed` |
| 408 | `request_timeout`, body deadline exceeded |
| 412 | `precondition_failed`; no mutation committed |
| 413 | `document_too_large`, including streamed/chunked bodies |
| 415 | `unsupported_media_type` |
| 500 | `storage_error`; detailed failure logged locally, not exposed to clients |
| 503 | `overloaded`, with `Retry-After: 1` |

Transport-level malformed HTTP, connection failures, or idle timeouts can be
handled by Hyper/Tokio before the application can emit a JSON error.

Durable commits complete **before** a successful write response is built.
However, a lost response, canceled request, process crash before receipt, or
I/O error during commit can leave the client uncertain whether the operation
committed. Atomicity still prevents torn document/metadata/revision updates.
Re-read a known UUID and use preconditions to reconcile. Do not blindly retry
POST: retry-safe POST idempotency keys are not implemented.

## Configuration and resource bounds

Configuration is environment-only; invalid settings and unknown `NANO_DB_*`
keys fail startup rather than falling back silently. Command-line arguments
are rejected.

| Environment variable | Default | Accepted range / constraint |
| --- | --- | --- |
| `NANO_DB_BIND` | `127.0.0.1:7070` | IP socket address, including bracketed IPv6; port 0 is allowed |
| `NANO_DB_PATH` | `nano-db.redb` | Nonempty file path; parent exists |
| `NANO_DB_AUTH_TOKEN` | unset | Required except explicit loopback development; 32..1024-byte bearer-token syntax |
| `NANO_DB_ALLOW_UNAUTHENTICATED` | `false` | Literal `true` or `false`; only effective without a token and on loopback |
| `NANO_DB_MAX_DOCUMENT_BYTES` | `1048576` | 1..67108864; counts all raw bytes |
| `NANO_DB_MAX_DEPTH` | `64` | 1..128; root object/array depth is 1, scalar depth is 0 |
| `NANO_DB_CACHE_BYTES` | `16777216` | 1048576..1073741824, redb cache budget |
| `NANO_DB_MAX_IN_FLIGHT` | `16` | 1..1024 active requests |
| `NANO_DB_MAX_QUEUED` | `64` | 0..4096 waiting requests |
| `NANO_DB_QUEUE_TIMEOUT_MS` | `100` | 1..60000 |
| `NANO_DB_BODY_TIMEOUT_MS` | `10000` | 1..300000, entire body read |
| `NANO_DB_MAX_CONNECTIONS` | `128` | 1..65536 accepted TCP connections |
| `NANO_DB_CONNECTION_IDLE_TIMEOUT_MS` | `30000` | 100..300000, no socket read/write progress |

Admission happens before reading JSON bodies. Saturated admission rejects
requests with 503 without constructing an unbounded application queue.
Request permits stay alive through response bodies and through blocking
transactions even if the caller disconnects. JSON validation and redb work
run on Tokio's blocking pool, with no more than the admitted request count
of jobs. redb permits concurrent read snapshots and serializes writers.

Accepted TCP connections are separately capped; excess connections wait in
the operating system's listen backlog rather than allocating service tasks.
Idle sockets are closed, and slow/incomplete JSON bodies hit their body
deadline. These are backpressure bounds, not a comprehensive rate limiter.
Hyper's HTTP parser imposes its own header bounds.

Memory use also includes runtime/connection overhead, decoded JSON keys during
validation, the redb cache, and copied response payloads. There is no claimed
fixed RSS ceiling. Lowering the write-size setting does not invalidate or
truncate already stored larger documents; they remain readable. Use sensible
combinations of cache size, document size, and concurrency.

## Storage, guarantees, and scope

The database has separate versioned redb tables for raw document bytes,
fixed-width metadata, and the database identity/revision counter. Writes and
deletes commit atomically using `Durability::Immediate`. Preconditions are
checked against current metadata while holding the same write transaction,
not in a preceding read. GET/HEAD use one consistent read snapshot.
On Unix, startup also syncs the parent directory before serving requests so
the newly created database's directory entry is covered by the durability
boundary. Filesystems that cannot sync the directory fail startup.

Durability depends on the filesystem, storage device, and operating system
honoring redb's sync operations. redb recovers its transactional state after
unclean shutdowns. A backend I/O failure is reported, never acknowledged as
success; redb can reject subsequent operations until the file is reopened.
This increment has no filesystem-capacity quota, full corruption scrub on
every read, automatic repair policy, schema migration tooling, or coordinated
multi-process writers. Do not modify the live database file externally.

TTL, POST idempotency keys, listing/pagination, backup/restore, replication,
secondary indexes, and document querying are deliberately deferred. There
is no automatic compaction/backup task, and no performance target has yet
been benchmarked.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests cover exact-byte/numeric round trips, UUIDs, decoded duplicate keys,
size/depth boundaries, content types, authentication, method/HEAD/204/304
semantics, conditional ordering, concurrent create-only/CAS requests,
consistent read snapshots, bounded queues/response lifetimes, canceled
blocking work, injected durable-commit failures, and real HTTP requests
across repeated process kills/restarts. Network tests also exercise chunked
limits, incomplete bodies, overload, idle connection admission, and graceful
shutdown while an accepted write is incomplete.

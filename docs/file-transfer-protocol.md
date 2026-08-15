# File-transfer protocol version 1

## Transport and framing

The protocol runs directly over TCP. It does not provide TLS, confidentiality, peer identity, or replay protection.

Each connection starts with the eight-byte magic `UTFT 01 00 00 00` (`UTFT`, protocol family byte `1`, then three zero bytes). The Client then writes a length-prefixed JSON `hello` control frame. Control frames use a big-endian `u32` byte length followed by UTF-8 JSON and are limited to 8 MiB. Both peers exchange major/minor versions; incompatible major versions are rejected.

After a successful hello, one request is served per TCP connection:

- `list`: lazily list one directory with a cursor, a maximum page size of 500, and a sort key.
- `stat`: obtain stable file identity, size, modification time, executable flag, and BLAKE3.
- `inspect`: obtain file-or-directory metadata without hashing file contents.
- `download`: request a file from an aligned verified offset and assert its expected identity.

All protocol paths are relative UTF-8 paths separated by `/`. Both peers reject absolute paths, `..`, backslashes, NULs, and resolved paths outside their configured root.

## Download stream

A successful `download` response repeats the current file identity, accepted offset, and chunk size. Data follows as:

```text
u32 chunk_length
[32] BLAKE3(chunk)
[chunk_length] raw bytes
```

The fixed maximum chunk size is 4 MiB. A zero chunk length ends data and is followed by a `complete` control frame. Completion is valid only when the Server confirms that source size and modification time did not change and the Client verifies the complete-file BLAKE3.

The Server transmits raw bytes without compression. A Client sends no file content; version 1 is download-only.

## Resume state

The Client stores data in `<destination>.utpart` and a small adjacent JSON metadata file. Metadata includes its own version, Server address, remote path, file identity, total size, negotiated chunk size, and last durable verified offset.

The Client verifies each block before writing. About every 64 MiB it synchronizes file data and advances the durable offset. On recovery, bytes beyond that offset are truncated. Missing, damaged, incompatible, or mismatched metadata is never guessed; explicit restart truncates the same partial file to zero.

The Server rejects a resume request whose expected identity differs. The Client retains a cancelled or disconnected partial file and retries transient failures three times by default.

## Authentication

When enabled, Server compares one shared token from the Client hello. All authenticated Clients have the same read-only access to the shared root. Because the connection is plaintext, the token is observable on the network and must not be considered secure authentication.

## Error behavior

Errors are bounded JSON control frames with a stable code and a human-readable message. Defined categories include protocol-version mismatch, authentication failure, busy Server, unsafe path, list/stat failure, invalid offset, changed source, and interrupted transfer.

The Server accepts multiple concurrent Client sessions up to its configured limit. A graceful shutdown stops accepting connections, completes the current chunk, marks active transfers interrupted, and gives Clients a resumable endpoint.

The Server and Client apply configurable idle timeouts while waiting for protocol data. Timeout, disconnect, and failed transfers are removed from the live status table and reported as interrupted or failed rather than remaining indefinitely active.

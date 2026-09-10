# Useful Tools (`ut`)

`ut` is a single command for a collection of small command-line tools. Running `ut` without arguments opens the interactive launcher; existing subcommands remain scriptable.

## File transfer quick start

Share the current directory over TCP:

```bash
ut file-transfer server
```

Share another directory or change the listener:

```bash
ut file-transfer server --dir /path/to/share --bind 0.0.0.0:9417
```

Open the interactive remote-file browser:

```bash
ut file-transfer client server.example.com:9417
```

Download files without the browser:

```bash
ut file-transfer client server.example.com \
  --remote "releases/app.zip" \
  --remote "documents" \
  --output ./downloads
```

The default Server port is `9417`. Both Server sharing and Client downloads default to the current directory.

When the Server runs in a terminal, each active download occupies one status row that is refreshed once per second. The remote path column is fixed-width; long names scroll within it, and active rows include an absolute local-clock ETA. Completed rows briefly show their final result and then disappear; redirected or non-interactive logs emit one final line per download instead of repeating progress lines. Client progress output also includes the estimated completion time.

### Optional authentication

Enable the shared-token gate:

```bash
ut file-transfer server --auth
```

The Server reads `UT_FILE_TRANSFER_TOKEN`, `--token-file`, or its saved token. When none exists in an interactive terminal, it generates a token and offers to save it. The Client reads the same environment variable or `--token-file`, and prompts when an authenticated Server rejects an unauthenticated connection. An entered Client token is saved only after confirmation or when `--save-token` is used.

> [!WARNING]
> File transfer protocol version 1 uses plaintext TCP. Authentication is only an access gate: network observers can read both the token and file contents. Do not treat it as protection on an untrusted network.

### Resumable downloads and storage

Downloads are written directly to `<name>.utpart` in the destination directory. A small `.meta.json` file records the remote identity and last durable offset. Every 4 MiB block is BLAKE3-verified; progress is made durable about every 64 MiB, and the complete file is verified before the partial file is atomically renamed.

This design never creates a second full-size copy. Before transfer, the Client requires only the remaining bytes plus a 100 MiB safety reserve. Cancelling keeps the verified partial data; selecting the same remote file later resumes it. Damaged metadata or a changed remote file requires explicit overwrite and restarts in the same partial file.

If a different destination already exists, Client refuses by default. Use `--overwrite` or toggle overwrite in the browser only when losing the old file is acceptable.

### File browser keys

| Key | Action |
| --- | --- |
| `↑` / `↓`, `j` / `k` | Move selection |
| `Enter` | Enter a directory or select a file |
| `Space` | Select/unselect a file or directory |
| `d` | Download selected items |
| `c` | Cancel the current batch and retain its breakpoint |
| `/` | Search the loaded directory page |
| `s` | Cycle name, size, and modification-time sorting |
| `n` | Load the next page (500 entries per page) |
| `r` | Refresh the directory |
| `O` | Explicitly toggle overwrite/restart permission |
| `Backspace` / `←` | Go to the parent directory |
| `q` | Quit when no download is active |

The browser supports multiple selection and recursive directory downloads. Empty directories are created. A failed file does not stop the rest of a batch; authentication, protocol, and disk-space failures stop the affected operation with a non-zero scripted exit.

### Cross-platform path behavior

Protocol paths are UTF-8 and use `/`. Absolute paths, parent traversal, and paths escaping either root are rejected. Symbolic links and special files are not shared. Names that are invalid or reserved on the Client platform are mapped to a readable name with a stable hash suffix; reversible mappings are recorded in `.ut-file-transfer-name-map.json`. Target collisions are detected before transfer instead of silently overwriting files.

Modification times are restored. The Unix executable bit is restored on macOS/Linux and ignored on Windows; ownership, ACLs, and other permissions are not copied.

## Other tools

```text
ut uuid-gen
ut base64 encode [FILE]
ut base64 decode [FILE]
ut file-server
ut http-echo
ut network-server
ut file-hash
ut file-gen <PATH> <SIZE>
ut clipboard [FILE]
```

Run `ut <tool> --help` for each tool's options.

The network server is useful for quickly testing clients that use more than
ordinary HTTP:

```bash
ut network-server
curl http://127.0.0.1:8090/healthz
curl -N http://127.0.0.1:8090/sse
```

Generate a file of an exact logical size for upload, transfer, or disk tests:

```bash
ut file-gen fixture.bin 10MiB
ut file-gen random.bin 1GB --random
ut file-gen fixture.bin 1000000 --force
```

Sizes accept bytes with no unit, decimal `KB`/`MB`/`GB`/`TB`, and binary
`KiB`/`MiB`/`GiB`/`TiB` units. Files are zero-filled by default and are written
in full rather than created as sparse files. Use `--random` for random bytes.
Existing files are protected unless `--force` is supplied; replacement happens
only after the new file is completely written.

It serves a browser smoke-test page at `/`, an SSE stream at `/sse`, a
WebSocket echo endpoint at `/ws`, and request-inspection JSON for all other
paths. SSE can be configured with `--sse-interval` (milliseconds),
`--sse-count` (`0` keeps the stream open), and `--sse-message`.

Base64 reads a file (or standard input when no file is supplied) and writes the
result to standard output. Literal text can be passed with `--text`; use
`--url-safe` for the URL-safe alphabet and `--no-padding` to omit or accept
unpadded data:

```bash
ut base64 encode --text "Hello, world!"
printf 'Hello, world!' | ut base64 encode
ut base64 decode encoded.txt > decoded.bin
```

Clipboard reads a file, or standard input when the file is omitted or `-`, and
copies the content to the system clipboard. It prints a success summary with
the source, size, and clipboard backend:

```bash
ut clipboard README.md
cat README.md | ut clipboard
ut file-hash --algorithm sha256 README.md | ut clipboard -
```

On Linux, `ut clipboard` uses the first available backend in this order:
`wl-copy`, `xclip`, then `xsel`. Install `wl-clipboard`, `xclip`, or `xsel` if
none of them is available. macOS uses `pbcopy`, and Windows uses `clip`.

## Shell tab completion

Automatically detect the current shell and install an idempotent startup hook:

```bash
ut completions install
```

Override detection when needed:

```bash
ut completions install --shell zsh
```

The installer supports Bash, Zsh, Fish, and PowerShell. It updates the corresponding user profile with a clearly marked managed block and regenerates completion code from the current `ut` command definition on every shell startup, so newly added commands and options appear automatically. Restart the shell or source the profile after installation.

Completion scripts can also be printed without modifying a profile:

```bash
ut completions bash
ut completions zsh
ut completions fish
ut completions powershell
```

## Build and test

```bash
cargo build --workspace
cargo test --workspace
```

GitHub Actions validates macOS, Linux, and Windows. Version tags build release archives for macOS Apple Silicon and Intel, Linux x86-64, and Windows x86-64.

The `ut` application version is `0.2.0`; file-transfer protocol versioning is independent. See [file-transfer protocol](docs/file-transfer-protocol.md).

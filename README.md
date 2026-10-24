# muse-cli (bản Rust)

Bản viết lại bằng Rust của `muse-cli` 0.3.2 (gốc Python) — công cụ dòng
lệnh nói chuyện với agent muse.ai cá nhân của bạn từ terminal. Code đồng
bộ (sync), không dùng Tokio, không chạy Python lúc runtime, không gọi
ngược bản Python gốc.

> Trạng thái: bản Rust tái tạo toàn bộ bề mặt CLI (50 leaf command),
> config, gateway Noise/protobuf, wait state machine và các helper thuần.
> Mọi hành vi cần gateway/cookie thật đều liệt kê ở mục
> [Chưa kiểm chứng live](#chưa-kiểm-chứng-live) — chưa có hành vi nào
> trong số đó được ghi là đã pass khi chưa chạy live.

## Trạng thái hiện tại (đo ngày 2026-10-24)

- Offline, không cần login hay mạng live: `cargo test` **203 pass /
  0 fail** (lib 150 + binary_cli 13 + fsclient 6 + library_pure 20 +
  mock_gateway 5 + waiter 9); `--version` in `muse-cli 0.3.2`;
  `--help` 39 lệnh top-level, 50 leaf (37 + `auth export` + 12 `fs`).
  Con số 205 trong ghi chú cũ là sai so với đo hiện tại, không dùng nữa.
- WebSocket qua egress proxy: `gateway.rs` đi qua proxy bằng tunnel
  HTTP CONNECT, tôn trọng `NO_PROXY`, proxy auth chỉ ở header CONNECT,
  không log (xem mục Khác biệt và `AGENTS.md`).
- `main()`: cài rustls ring CryptoProvider (tránh panic exit 101 khi
  live HTTPS) và khôi phục SIGPIPE mặc định — khối `unsafe` duy nhất
  trong production, xem `AGENTS.md`.
- `fs`: chunk 4 MiB, `walk` với depth `None` là không giới hạn,
  `fs tree` mặc định depth 2, không có cap byte tổng thể.
- Live read-only 11/11 đã có từ phiên trước (tài khoản test,
  2026-10-24, không chạy lại live trong đợt xuất bản này): `status`,
  `threads`, `history --limit 5`, `usage`, `invite` không `--create`,
  `feed --limit 5`, `feed-status`, `feed-prompt`, `goals`, `ideas`,
  `unread`.
- Còn mở: `update` tự nâng cấp (hiện check-only), live write
  (`send`/`wait`, `fs put`/`rm`/`write`, `invite --create`, `redeem`,
  `approve`), chat tương tác, `auth export` với Chrome thật — xem mục
  Chưa kiểm chứng live và `AGENTS.md`.

## Build

Yêu cầu: Rust toolchain ổn định (cargo, rustc). Nếu `cargo` chưa có
trong `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Build debug (binary dùng để test):

```sh
cargo build
```

Build release:

```sh
cargo build --release
```

Binary nằm ở:

- Debug: `target/debug/muse-cli`
- Release: `target/release/muse-cli`

Chạy trực tiếp bằng đường dẫn đó, ví dụ:

```sh
./target/debug/muse-cli --version
./target/debug/muse-cli status
```

**Không cài toàn cục.** Task này cấm `cargo install` và cấm ghi đè
binary hệ thống; đừng copy binary vào `/usr/local/bin` hay
`~/.local/bin`. Nếu muốn gọi ngắn, dùng đường dẫn đầy đủ hoặc alias
shell của riêng bạn trỏ tới `target/debug/muse-cli` /
`target/release/muse-cli`.

## Chạy test

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test
```

Chạy riêng từng nhóm integration test:

```sh
cargo test --test binary_cli    # binary: --help/--version, lỗi parse, chưa login
cargo test --test library_pure  # library thuần: cookie, auto-approve, media, routes, chunk
cargo test --test mock_gateway  # mock gateway: unary + subscribe qua transport in-memory
cargo test --test waiter        # Waiter state machine thuần
cargo test --test fsclient      # fs thuần: norm, CHUNK 4 MiB, fmt_size, base64, replace
```

Test không cần login thật, không gọi mạng live, không dùng secret hay
cookie thật:

- Test binary chạy binary thật với `HOME` trỏ tới thư mục tạm trống
  (không chạm `HOME` thật hay `~/.config` thật).
- Test mock gateway tự viết fake responder trong `tests/` bằng `snow`
  (Noise XX responder) + `prost` (encode `ServiceFrame` /
  `ApplicationResponse` / `BodyChunk`) qua channel pair in-memory, nối
  vào `Gateway::connect_with_transport`. Không dùng helper test nội bộ
  của `gateway.rs`.
- Fixture chỉ dùng giá trị giả rõ ràng (`hatch_sess=FAKE-…`).

Lưu ý: test binary cho các lệnh cần dispatch (bare `auth`/`fs`,
`approve` thiếu id/`--all`, `fs rm` thiếu `--yes`, `raw` sai đầu vào,
`status` chưa login) kiểm tra hành vi của `dispatch` — các check
pre-connect này phải chạy trước khi kết nối gateway.

## Danh sách lệnh (50 leaf)

Quy ước đếm: 39 lệnh top-level, trong đó `auth` và `fs` là lệnh cha;
leaf = 37 lệnh thường + `auth export` + 12 lệnh con của `fs` = 50.
Chạy `muse-cli <lệnh> --help` để xem cờ của từng lệnh. Không lệnh nào
đồng nghĩa với `chat`.

### Chat và trạng thái

| Lệnh | Ý nghĩa |
|---|---|
| `chat` | Chat tương tác (mặc định khi không có lệnh). Lệnh slash: `/help`, `/?`, `/chats`, `/open N`, `/main`, `/new [title]`, `/history [N]`, `/login`, `/exit`, `/quit` (cả `exit`/`quit` không slash cũng thoát). |
| `status` | VM, số session, unread, identity (JSON). |
| `usage [--json] [--raw]` | Plan usage: % limit đã dùng, ngày reset, bonus token. `--raw` thắng `--json`. |
| `wake` | Yêu cầu đánh thức VM (JSON `{"status", "body"}`). |

### Gửi, chờ, lịch sử

| Lệnh | Ý nghĩa, cờ chính |
|---|---|
| `send <TEXT> [--thread ID] [--wait N] [--until-done] [--text] [--ack-timeout N] [--download DIR] [--auto-approve\|--no-auto-approve]` | Gửi message. `--wait` mặc định 90 (0 = chỉ chờ delivery ack), `--until-done` = 86400s và thắng `--wait`, `--ack-timeout` mặc định 30, `--download` chỉ chạy khi thời gian chờ khác 0. |
| `wait [--thread ID] [--after-seq N] [--timeout N] [--json] [--require-running] [--auto-approve\|--no-auto-approve]` | Chờ turn kết thúc. `--timeout` mặc định 600. Exit: 0 done, 2 failed, 4 needs human, 124 timeout. |
| `history [--thread ID] [--limit N] [--raw] [--download DIR]` | Đọc message. `--limit` mặc định 10. |
| `threads [--all]` | Liệt kê chat và side chat. Mặc định bỏ archived; `--all` giữ cả archived. |
| `watch [--timeout N] [--n N] [--all] [--thread ID]` | Tail live event, mỗi event một dòng JSON. `--timeout` mặc định 60, `--n` mặc định 50. Mặc định bỏ event `agent.status`; `--all` giữ lại. |

### Approval

| Lệnh | Ý nghĩa |
|---|---|
| `approvals [--recent] [--raw] [--approve-all]` | Liệt kê approval đang chờ. `--approve-all` thắng cả `--raw`/`--recent`. |
| `approve [ID] [--all] [--once\|--session\|--today\|--always]` | Duyệt một approval hoặc tất cả. Bốn cờ decision mutually exclusive, mặc định `allow_once`. Thiếu cả `ID` và `--all`, hoặc có cả hai, đều lỗi exit 2. |
| `deny <ID> [--always]` | Từ chối (`deny`, hoặc `deny_always` với `--always`). Không có `--all`. |
| `auto-approve [STATE]` | `on`/`always`/`off`/`status` (mặc định `status`). `on` và `always` đều ghi `auto_approve = "always"` vào config; luôn in JSON gồm `auto_approve`, `source`, `config`, `env`. |

### File và media

| Lệnh | Ý nghĩa |
|---|---|
| `files [--thread ID] [--limit N]` | Liệt kê file (ảnh/video) trong message gần đây. `--limit` mặc định 20. |
| `download <REF> [-o DIR] [--thread ID] [--limit N] [-f/--force]` | Tải file agent tạo ra. `REF` là `sandbox://…`, URL chứa `/media/raw/`, message id hoặc history seq. `-o` mặc định `.`, `--limit` mặc định 50. Không tìm thấy file → exit 1; có row tải lỗi → in JSON trước rồi exit 3. |

### Feed, goals, ideas

| Lệnh | Ý nghĩa |
|---|---|
| `feed [--limit N] [--raw]` | Liệt kê feed unit (phẳng từ `days[].editions[].units[]`). `--limit` mặc định None; `--limit 0` được coi như không có limit (falsy, giống bản gốc). |
| `feed-status` | Trạng thái refresh feed (JSON). |
| `feed-react <UNIT> [REACTION]` | React cho feed unit, `REACTION` mặc định `love`, không validate giá trị. |
| `feed-prompt` | Feed prompt (JSON). |
| `goals` | Liệt kê goal. |
| `goal <ID>` | Xem một goal. |
| `ideas` | Liệt kê idea card. |
| `idea <ID>` | Xem một idea. |
| `idea-exec <ID> [--thread ID]` | Chạy một idea (body luôn có `ideaCardId` và `mode: "full"`, thêm `session_id` nếu có `--thread`). |
| `unread` | Số chưa đọc (JSON). |
| `seen <THREAD>` | Đánh dấu thread đã xem. |

### Session

| Lệnh | Ý nghĩa |
|---|---|
| `session-start [--title TITLE]` | Tạo side chat mới. |
| `session-rename <ID> <TITLE>` | Đổi tên session. |
| `session-pin <ID>` | Pin session. |
| `session-unpin <ID>` | Unpin session. |
| `session-archive <ID>` | Archive session. |
| `session-unarchive <ID>` | Unarchive session. |
| `session-delete <ID>` | Xóa session. |

### Invite

| Lệnh | Ý nghĩa |
|---|---|
| `invite [--json] [--raw] [--create]` | Code invite của bạn, lượt dùng còn lại, reward. `--raw` thắng `--json`; `--create` chỉ tạo khi chưa có code (no-op nếu đã có). |
| `redeem <CODE> [--json]` | Redeem code invite của bạn bè (trong 48h sau khi join). `--json` in JSON kể cả khi thất bại; không `--json` mà thất bại thì in TEXT và exit 1; code sai định dạng thì exit 2. |

### Auth, update, escape hatch

| Lệnh | Ý nghĩa |
|---|---|
| `auth export` | Copy login muse.ai từ Chrome (cần remote debugging bật và tool `agent-browser`) vào `~/.config/muse-cli/cookies.txt` (mode 600). Thành công in ra stdout; thất bại in ra stderr, exit 1. `auth` không có subcommand → exit 2. Ghi file an toàn hơn gốc: file cũ → `.bak` set 600 ngay, file mới tạo đã 600 từ đầu, `.bak` chỉ xóa sau khi ghi xong (ghi lỗi thì `.bak` còn ở 600 để khôi phục tay). |
| `update` | **CHECK-ONLY ở bản Rust** (xem mục Khác biệt): báo version mới nhất nếu lấy được và in hướng dẫn nâng cấp, không tự tải/cài/ghi đè binary. Lệnh `update` không chạy update-check nền. |
| `raw <METHOD> [--body JSON] [--param K=V ...] [--timeout N]` | Gọi bất kỳ gateway method nào. `--timeout` mặc định 30. Method lạ, `--body` không phải JSON, `--param` thiếu `=` đều exit 2 trước khi kết nối. |

### System Files (`fs`)

`fs` không có subcommand → exit 2. Mọi lệnh `fs` đều cần login.
Đường dẫn được chuẩn hóa về dạng tương đối với root (`""` hoặc `/`
là root, bỏ prefix `sandbox://`, `\` thành `/`); `..` được collapse
qua normpath ngay tại root (ví dụ `../secret` → `secret`), nên
check `..` sau chuẩn hóa không fire — đây là parity với bản gốc,
không phải chặn exit 2. Đọc/ghi file theo chunk 4 MiB. `fs get`
kiểm tra thêm bằng symlink/canonicalize: đích cuối thoát khỏi thư
mục tải (kể cả qua symlink trồng sẵn) thì bị từ chối.

| Lệnh | Ý nghĩa, cờ chính |
|---|---|
| `fs ls [PATH] [-l] [-a] [--json]` | Liệt kê thư mục (mặc định root). `-l/--long` in dài, `-a/--all` giữ dotfile. |
| `fs tree [PATH] [--depth N] [-a] [--json]` | Liệt kê đệ quy. `--depth` mặc định 2 (root + 1 cấp con). Lọc dotfile theo bất kỳ thành phần nào của đường dẫn. |
| `fs stat <PATH>` | Metadata file/thư mục (JSON). |
| `fs cat <PATH> [--max-bytes N] [--binary]` | In bytes thô ra stdout. File binary (8192 byte đầu chứa `\0`) mà không `--binary` → stderr, exit 1. |
| `fs get <PATH> [-o DIR] [-r]` | Tải file (thư mục cần `-r`). `-o` mặc định `.`. Thư mục không `-r` → exit 2. |
| `fs find <NAME> [--limit N]` | Tìm theo substring tên. `--limit` mặc định None; `--limit 0` = không giới hạn. |
| `fs put <LOCAL> <REMOTE> [-r] [-n]` | Upload file (thư mục cần `-r`). `-n/--no-clobber` → không ghi đè. |
| `fs write <REMOTE> [--append] [-n]` | Ghi stdin vào file remote. `--append` đọc file đích rồi ghi nối toàn bộ. |
| `fs edit <REMOTE> [--replace OLD NEW ...]` | Sửa file bằng `$VISUAL`/`$EDITOR`/`vi` (xem mục Khác biệt: exec trực tiếp, không qua shell) hoặc `--replace` lặp lại được. `--replace` không tìm thấy text → exit 2; nội dung không đổi → không ghi. |
| `fs mkdir <PATH>` | Tạo thư mục (luôn tạo parents). |
| `fs mv <SRC> <DST> [-f]` | Move/rename (`-f` ghi đè đích). |
| `fs rm <PATH> [-r] [--yes]` | Xóa. **Thiếu `--yes` → stderr `refusing to delete without --yes`, exit 2, chưa hề kết nối.** |

## Config và biến môi trường

### File

| Đường dẫn | Nội dung |
|---|---|
| `~/.config/muse-cli/cookies.txt` | Cookie login. `auth export` ghi dạng Netscape, mode 600. Thiếu file → lỗi auth, exit 2; file rỗng → lỗi auth. |
| `~/.config/muse-cli/config.json` | `cookies_file` (mặc định `<config_dir>/cookies.txt`), `vm_id`, `node_id`, `auto_approve` (`"always"`/`"off"`), `subscription_action_id` (do `usage` cập nhật). |
| `~/.config/muse-cli/chat.json` | State chat tương tác: `{"session_id", "title"}`. |
| `~/.config/muse-cli/update.json` | State update-check nền: `{"checked_at", "latest"}`. |

Thư mục config là `~/.config/muse-cli` theo home expansion, **không**
dùng `XDG_CONFIG_HOME` (bản gốc cũng không). `load_config` đọc
`config.json` nếu có; nếu chưa có `node_id` thì sinh 16 ký tự hex và
lưu ngay (tạo thư mục nếu cần). `save_config` ghi nguyên tử qua file
tmp + rename.

Cookie parse chấp nhận 3 dạng, theo đúng thứ tự bản gốc: JSON object
(có dict `cookies`, hoặc toàn bộ top-level dict), chuỗi `a=b; c=d`
(khi nhắc tới `hatch_sess=` hoặc `datr=`), jar Netscape. Không parse
được → lỗi auth.

### Biến môi trường

| Biến | Tác dụng |
|---|---|
| `MUSE_VM_ID` | Seed `config.vm_id` khi key chưa có (không tự lưu file nếu chỉ có seed này). |
| `MUSE_NODE_ID` | Seed `config.node_id` khi key chưa có; `send`/`chat` gửi `node_id` này trong body `chat.stream`. |
| `MUSE_AUTO_APPROVE` | Override auto-approve cho `wait`/`send`. Bật: `always`, `on`, `1`, `true`, `yes`; tắt: `off`, `0`, `false`, `no`, `never`, `""` (chuỗi rỗng vẫn tính là "đã set"). Giá trị khác → lỗi Value, exit 2. |
| `MUSE_NO_UPDATE_CHECK` | Set với giá trị khác rỗng (kể cả `"0"`) → tắt update-check nền; chuỗi rỗng coi như chưa set (giống `os.environ.get()` falsy của Python). |
| `CI` | Set với giá trị khác rỗng → tắt update-check nền; chuỗi rỗng coi như chưa set. |
| `VISUAL`, `EDITOR` | `fs edit` không `--replace`: `VISUAL` trước, rồi `EDITOR`, rồi `vi`. |

### Thứ tự ưu tiên auto-approve

**Flag > env > config > off.** Flag là `--auto-approve` /
`--no-auto-approve` trên `send`/`wait` (mutually exclusive). Config chỉ
tính là bật khi giá trị (lowercase) đúng bằng `"always"` — config
`"on"` không bật. Lưu ý bẫy đã biết: nếu env đang `off` mà bạn vừa
chạy `auto-approve on` (ghi config `always`), output vẫn báo
`auto_approve: "off", source: "env MUSE_AUTO_APPROVE",
config: "always"` — đó là hành vi đúng theo thứ tự ưu tiên, không phải
bug. `approvals --approve-all` và `approve --all --always` không dùng
thứ tự này mà decide trực tiếp.

### Update-check nền

Chỉ chạy khi **cả** stdout và stderr đều là tty, và cả hai biến
`MUSE_NO_UPDATE_CHECK`, `CI` đều không set với giá trị khác rỗng
(chuỗi rỗng coi như chưa set). Lệnh `update` không chạy
check nền. Cache 24h trong `update.json`; fetch lỗi thì backoff 1h
(giữ version cũ). Thread nền được join tối đa 1.5s; nếu có bản mới hơn
theo `x.y.z` toàn số (pre-release/chữ cái coi như không có) thì in
notice ra **stderr**. Check nền không bao giờ làm lệnh thất bại và
không bao giờ tự nâng cấp.

## Exit code

| Tình huống | Exit |
|---|---|
| Thành công | 0 |
| Lỗi auth (`auth error: …`), lỗi Value (`error: …`), lỗi I/O | 2 |
| Lỗi gateway (`gateway error: …`) | 3 |
| Timeout tầng gateway (`timeout: …`) | 4 |
| `wait`/`send`: turn failed | 2 |
| `wait`/`send`: needs human (needs input hoặc needs approval) | 4 |
| `wait`/`send`: timeout (turn chưa xong) | 124 |
| `send`: không có delivery ack trong `--ack-timeout` | 5 |
| `auth export` thất bại / thiếu `agent-browser` / thiếu `hatch_sess` | 1 |
| `redeem` không `--json` mà thất bại | 1 |
| `download` không tìm thấy file cho `REF` | 1 |
| `download` có row tải lỗi (đã in JSON trước) | 3 |
| `fs cat` file binary mà không `--binary` | 1 |
| `fs rm` thiếu `--yes`, `raw` sai đầu vào, `approve` thiếu cả id và `--all` hoặc có cả hai, lỗi parse (clap, thay argparse) | 2 |
| Lỗi parse: lệnh lạ, thiếu đối số bắt buộc, cờ xung đột, giá trị sai kiểu/choice | 2 |
| `--help`, `--version` | 0 |

Lưu ý: lỗi parse ở bản Rust do clap in ra, wording khác argparse của
bản gốc nhưng exit code vẫn là 2. Xem thêm mục Khác biệt.

## Cấu trúc thư mục

```text
muse-cli-rust/
├── Cargo.toml          # package muse-cli-rust, bin muse-cli 0.3.2, lib muse_cli
├── AGENTS.md           # hướng dẫn cho agent mới vào repo
├── LICENSE             # MIT, như bản gốc
├── .gitignore          # loại target/, *.log, secret và config thật
├── CONTRACT.md         # hợp đồng nội bộ giữa các writer (không phải doc người dùng)
├── README.md           # file này
├── docs/               # kiến trúc, CLI, gateway, parity
├── scripts/checks/     # linkage checks chạy lại được (commands-help, proto-descriptors, routes-called)
├── assets/
│   ├── routes.json     # 258 gateway method (nhúng vào binary lúc build)
│   ├── desc0.bin       # descriptor noise_transport.proto
│   └── desc1.bin       # descriptor noise_envelope.proto
├── src/
│   ├── main.rs         # entry point: parse, update-check nền, map lỗi -> exit code
│   ├── lib.rs          # khai báo module
│   ├── cli.rs          # bề mặt clap: 39 top-level, 50 leaf, flag/default
│   ├── config.rs       # config, cookie parse 3 dạng, auto-approve precedence
│   ├── errors.rs       # CliError: Auth/Gateway/Value/Timeout/Io
│   ├── output.rs       # in JSON pretty indent 2
│   ├── routes.rs       # bảng route 258 + build_request (thay path param, query, header)
│   ├── proto.rs        # struct prost, field number đúng desc*.bin
│   ├── noise.rs        # Noise XX initiator (snow), static key ngẫu nhiên mỗi kết nối
│   ├── gateway.rs      # HTTPS auth, WebSocket + Noise, unary/subscribe, call_json, media.raw
│   ├── wait.rs         # Waiter state machine thuần + run_wait
│   ├── usage.rs        # parse_tokens_left, summarize_usage/invite, RSC parse
│   ├── media.rs        # media_path, event_resources, local_name (phần thuần)
│   ├── update.rs       # update-check nền + lệnh update CHECK-ONLY
│   ├── commands.rs     # dispatch từng lệnh
│   ├── chat.rs         # chat tương tác
│   └── fsclient.rs     # System Files client (chunk 4 MiB, chuẩn hóa path)
└── tests/
    ├── binary_cli.rs   # binary thật: version/help/parse/chưa login
    ├── library_pure.rs # library thuần + proto cross-check với desc*.bin
    ├── mock_gateway.rs # fake responder snow+prost qua transport in-memory
    ├── waiter.rs       # Waiter thuần
    └── fsclient.rs     # fs thuần: norm, CHUNK, fmt_size, base64, replace
```

Kiến trúc gateway (tóm tắt): HTTPS `muse.ai` lấy token
(`api/auth/check` → `api/hatch/token`, và `api/session` trước nếu chưa
có `vm_id`), rồi WebSocket tới `hatch.metaaivm.com` + Noise XX handshake
3 message payload rỗng, stream id bắt đầu từ 1 và tăng đơn điệu. Unary
request bỏ frame của stream khác (không buffer); response/body_chunk
tích lũy tới `end_body`; reset → `Gateway(-1)`; không có response tới
deadline → Timeout; status ngoài 200..300 → Gateway với payload cắt
500 byte; quirk giữ lại từ bản gốc: đã thấy response head nhưng
`end_body` không tới trước deadline thì trả phần đã tích lũy, không
báo lỗi. Subscribe: reader thread + channel, poll 2s, idle timer chỉ
reset bởi frame của chính stream đó, record là dòng non-blank sau khi
tách newline, `on_record` trả true thì dừng, buffer cuối non-blank
thành record cuối, reset/`end_body` với buffer rỗng thì dừng không lỗi.
`call_json`: payload rỗng → `{}`, JSON sai → `Gateway(-1,
"non-JSON response: …")`, dict `ok == false` → `Gateway(-1,
"api error: …")`, dict có `result` → giá trị đó, còn lại → toàn bộ dict.

## Khác biệt / chưa tái tạo được

Mục này liệt kê mọi chỗ bản Rust cố tình khác bản gốc, hoặc chưa tái
tạo được — không có chỗ nào bị lặng lẽ bỏ qua.

- **`update` check-only, không tự nâng cấp.** Bản gốc: lệnh `update`
  nâng cấp thật qua `uv`/`pipx`/`pip` (trừ source checkout thì chỉ in
  `git pull`), exit code bằng exit code của tiến trình nâng cấp. Bản
  Rust: lệnh `update` chỉ báo version mới nhất (nếu lấy được) và in
  hướng dẫn nâng cấp cho đúng loại install (checkout/uv/pipx/pip),
  không tải, không cài, không ghi đè binary. Đây là partial
  implementation có chủ ý theo CONTRACT, và chính output của lệnh cũng
  nói rõ nó không tự nâng cấp.
- **WebSocket qua egress proxy.** `tungstenite` nối trực tiếp, khác
  `curl_cffi` của bản gốc (tôn trọng proxy env), nên ở host chỉ ra
  ngoài qua HTTP egress proxy thì handshake thất bại. Bản Rust đi qua
  proxy bằng tunnel HTTP CONNECT rồi mới TLS + WebSocket trên tunnel
  đó; tôn trọng `NO_PROXY`, proxy auth chỉ ở header CONNECT, không
  log, không đưa vào error message, không gửi tới gateway host. Không
  có proxy thì code này không hoạt động gì thêm, đường nối trực tiếp
  giữ nguyên.
- **SIGPIPE mặc định và rustls ring provider ở `main()`.** Rust bỏ qua
  SIGPIPE lúc khởi động nên `println!` vào pipe đã đóng từng panic
  exit 101; `main()` khôi phục disposition mặc định (shell báo 141
  như CLI Unix bình thường) — đây là khối `unsafe` duy nhất trong
  production. Tương tự, live HTTPS từng panic exit 101 vì rustls 0.23
  không tự chọn CryptoProvider, nên `main()` cài ring provider một
  lần trước mọi network use.
- **Chrome TLS fingerprint không impersonate được.** Bản gốc dùng
  `curl_cffi impersonate="chrome"` cho HTTPS/WebSocket. `reqwest` /
  `tungstenite` không impersonate Chrome được, nên bản Rust gửi TLS
  fingerprint của Rustls. Live parity vì vậy chưa chứng minh được
  (xem mục Chưa kiểm chứng live) — không claim là tương đương.
- **`fs edit` exec trực tiếp thay `shell=True`.** Bản gốc mở editor qua
  shell với tên file temp được `shlex.quote`. Bản Rust exec
  `$VISUAL`/`$EDITOR`/`vi` trực tiếp, không qua shell (an toàn hơn,
  hành vi với editor là shell builtin hoặc lệnh phức tạp có thể khác).
- **`wake` thiếu cookies → Auth/exit 2 thay traceback/exit 1.** Bản gốc:
  `wake` không kiểm tra cookies file trước, `open()` thiếu file raise
  `FileNotFoundError` không nằm trong `except` của `main()` → traceback,
  exit 1. Bản Rust chuẩn hóa thành `auth error`, exit 2 (CONTRACT).
  Tương tự, `load_cookies` trên file thiếu cũng trả Auth thay vì
  `FileNotFoundError`.
- **`send --text` tách dest.** Bản gốc: argparse dùng chung dest `text`
  cho positional `text` và flag `--text` (store_true), nên
  `send "hello" --text` gửi boolean `True` làm message, còn
  `send --text "hello"` gửi `"hello"` nhưng mất text mode. Bản Rust
  tách riêng: positional luôn là message, flag `--text` luôn chọn
  output text. Đây là sửa hành vi bug, có ghi rõ.
- **Clap help/error wording khác argparse.** Tên lệnh, cờ, default,
  required và exit code giữ nguyên; chỉ wording/format của help và
  thông báo lỗi parse khác (clap thay argparse). Đây là khác biệt được
  chấp nhận theo CONTRACT. Riêng lệnh `help` tự sinh của clap đã tắt
  ở cả 3 cấp (top-level, `auth`, `fs`) từ fix round 1, nên `--help`
  top-level đúng 39 lệnh thật, `auth --help` chỉ có `export`,
  `fs --help` chỉ có 12 lệnh — lệch này không còn.
- **`wait`/`send` JSON có `status` thật.** Từ fix round 1,
  `WaitResult` mang đủ `status`, `replies`, `after_seq`, `widgets`,
  `needs_input_reason` như dict `run_wait` của Python, và
  `wait`/`send` in đúng các field đó (không còn re-read history lần
  thứ hai sau `run_wait` như trước). `status` là task status cuối
  của root agent mà `Waiter` thấy (`completed`, `failed`, …);
  `null` chỉ khi chưa từng thấy task status nào (ví dụ timeout
  thuần, chưa có event nào), và text mode khi đó in `(status=None)`
  đúng như Python in `w.status is None`.
- **`watch` row dùng separator của Python.** Mỗi dòng JSON dùng
  separator mặc định của `json.dumps` (`', '` và `': '`, có space
  sau dấu phẩy và dấu hai chấm). Việc chèn space chỉ làm ngoài
  string JSON (tracking in-string/escape), nên giá trị string không
  bao giờ bị chèn space.
- **Config JSON hỏng → Value/exit 2.** Bản gốc: `load_config` dùng
  `json.load` trực tiếp, config hỏng → traceback, exit 1. Bản Rust:
  JSON hỏng, top-level không phải object, hoặc key đã biết sai kiểu →
  `CliError::Value`, exit 2, thông báo gọn.
- **`save_config` bỏ key lạ.** Bản gốc round-trip dict nên giữ key
  không biết do người dùng tự thêm vào `config.json`. Bản Rust chỉ
  serialize các field đã biết (`cookies_file`, `vm_id`, `node_id`,
  `auto_approve`, `subscription_action_id`); key lạ tự thêm sẽ mất khi
  `save_config` chạy (chỉ `auto-approve` dùng `save_config`; đường
  `load_config` khi sinh `node_id` cũng ghi lại toàn bộ struct).
- **JSON key order sorted.** `serde_json::Map` sắp xếp key theo thứ tự
  alphabet, trong khi Python giữ thứ tự server gửi. JSON serialize ra
  vì vậy có key order khác bản gốc; thứ tự trong list được giữ nguyên,
  và ngữ nghĩa JSON không đổi.
- **`subscription_action_id` có thể đã cũ + cơ chế fallback.** Id mặc
  định `4029ac5c4bd03136487d91ebde5de39c64b35229b4` ghi nhận tới
  2026-10-23, có thể đã cũ khi web app redeploy. `usage` thử theo thứ
  tự: id đã lưu trong config → id mặc định → quét JS bundle tìm
  `fetchSubscriptionAction` rồi thử lại; chỉ khi cả ba đều thất bại mới
  báo `Gateway(-1)`. Id còn sống hay không vẫn thuộc mục Chưa kiểm
  chứng live.
- **WebSocket connect không có timeout 20s.** Bản gốc truyền
  `timeout=20` cho WS connect; `tungstenite::connect` không nhận
  timeout nên bản Rust không enforce connect timeout ở bước này
  (handshake và read sau đó vẫn blocking như bản gốc).
- **`wake` với body non-JSON không rỗng → body null.** Bản gốc:
  `resp.json()` raise (subclass `ValueError`) → exit 2. Bản Rust trả
  `{"status", "body": null}` để `wake` vẫn in được JSON hữu ích.
- **`find_action_id` quét bundle tuần tự.** Bản gốc dùng pool 16
  thread; bản Rust fetch từng chunk một. Kết quả (id đầu tiên tìm thấy,
  hoặc None sau 3000 file) giống nhau, chỉ chậm hơn.
- **`run_wait` history re-read nay khớp Python (deviation cũ đã gỡ
  từ fix round 2).** Trước đây, factory gateway cho bước đọc lại
  history mà thất bại thì replies fallback về rỗng. Nay: lỗi connect
  ở bước này propagate như Python (connect nằm ngoài `try`), chỉ
  lỗi `call_json` dạng Gateway/Timeout mới fallback về replies rỗng.
- **`auth export` ghi cookies an toàn hơn gốc (khác biệt có chủ ý,
  TỐT HƠN gốc).** Bản gốc (`cli.py:331-334`): file cũ → `.bak`,
  ghi file mới với umask rồi mới `chmod 600`, `.bak` không bao giờ
  được set 600 hay xóa. Bản Rust: `.bak` set 600 ngay sau rename,
  file mới tạo đã 600 từ lúc tạo (không qua `fs::write` rồi chmod
  sau), `.bak` chỉ xóa sau khi ghi thành công; ghi thất bại thì
  `.bak` còn ở 600 để khôi phục tay.
- **Lỗi transport/Noise/parse của Rust map thành Io, exit 2
  (chuẩn hóa có chủ ý).** Bản gốc: các lỗi này không nằm trong
  `except` của `main()` → traceback, exit 1. Bản Rust: chúng thành
  `CliError::Io`, in `error: …`, exit 2 như mọi lỗi I/O khác.
- **Chỉ `-V` bị từ chối, `--version` vẫn hoạt động.** Bản gốc
  (argparse) chỉ có `--version`; clap tự thêm `-V` nên bản Rust tắt
  cờ `-V` và khai lại `--version` tường minh: `-V` → exit 2,
  `--version` in `muse-cli 0.3.2`, exit 0.
- **`WaitResult` thêm field `approvals` (và từ fix round 1:
  `status`, `replies`, `after_seq`, `widgets`,
  `needs_input_reason`).** CONTRACT gốc chỉ có `approvals` là thêm
  ngoài (writer B thêm để writer C in được JSON needs-approval từ kết
  quả `run_wait`, rỗng trừ khi outcome là `NeedsApproval`); các field
  còn lại là khớp dict `run_wait` của Python, không phải thêm mới so
  với bản gốc. Không phá reader nào của struct.
- **Local-time của `usage` best-effort.** Python format `resets_at`
  theo timezone local của hệ thống. Rust std không có API local-offset
  và crate không có `chrono`, nên `format_local_timestamp` đọc offset
  từ TZif (`/etc/localtime`, hoặc `/usr/share/zoneinfo/<TZ>` khi `TZ`
  đặt tên zone) với fallback kiểu POSIX, mặc định UTC khi không parse
  được. Format với offset tường minh thì được test đầy đủ.
- **Quirk `vm_id: null` trong config.** Key `vm_id` có mặt với giá trị
  null được tính là "key đã có" nên không seed từ `MUSE_VM_ID`
  (khớp check `"vm_id" not in cfg` của Python); các key null khác được
  tính là absent. `node_id: null` được tính là thiếu (CONTRACT định
  kiểu `node_id` là `String`).
- **Các khác biệt khác đã biết từ survey (code thắng README gốc):**
  `auto-approve` chấp nhận cả 4 giá trị `on`/`always`/`off`/`status`
  (README gốc thiếu `always`); `send --download` chạy khi thời gian chờ
  khác 0 (cả `--until-done` cũng thỏa, `--wait 0` thì không), không
  phải chỉ khi `--wait` được truyền; update-check nền yêu cầu **cả**
  stdout và stderr là tty và `MUSE_NO_UPDATE_CHECK`/`CI` set với giá
  trị khác rỗng (kể cả `"0"`) thì tắt, chuỗi rỗng coi như chưa set;
  `threads` mặc định bỏ archived;
  `history --limit` mặc định 10 (không phải 5 như ví dụ README gốc);
  `feed --limit` mặc định None, `files --limit` mặc định 20,
  `download --limit` mặc định 50, `-o` mặc định `.`; `approve` mặc
  định `allow_once`; các ngoại lệ "không phải JSON": `usage`/`invite`/
  `redeem` không flag in TEXT, `send --text` in TEXT, `watch` in từng
  dòng JSON, `fs ls`/`tree` không `--json` in TEXT, `fs cat` in bytes
  thô, `auth export` thành công in TEXT, `update` in stderr TEXT.
- **Đã có `commands.rs` (3187 dòng) implement theo CONTRACT +
  cli-surface, nên ghi chú cũ "file này chưa có khi viết README" không
  còn đúng:** format output chi tiết của từng lệnh gateway, hành vi
  chat tương tác end-to-end (banner, poll 1s, WAIT 300s, QUIET 10s),
  số call chunk của `FsClient::read_file`/`write_file` qua mock
  gateway, và wording chính xác của thông báo lỗi dispatch (trừ các
  message đã cố định trong test: `refusing to delete without --yes`,
  `unknown method '…'`, `give an approval ID or --all (not both)`)
  vẫn thuộc mục Chưa kiểm chứng live — chưa đối chiếu live từng dòng,
  không coi là final ở phần live.

## Chưa kiểm chứng live

Mọi mục dưới đây cần gateway/cookie thật và **chưa được kiểm chứng** —
trừ 11 lệnh read-only đã pass ghi ở mục Trạng thái hiện tại (`status`,
`threads`, `history`, `usage`, `invite` không `--create`, `feed`,
`feed-status`, `feed-prompt`, `goals`, `ideas`, `unread`). Các lệnh
còn lại trong danh sách dưới — kể cả mọi lệnh write — vẫn chưa được
kiểm chứng, không mục nào trong số đó được ghi là pass:

- Mọi lệnh cần gateway thật: `status`, `threads`, `history`, `send`,
  `wait`, `watch`, `feed`, `feed-status`, `feed-react`, `feed-prompt`,
  `goals`, `goal`, `ideas`, `idea`, `idea-exec`, `unread`, `seen`,
  `session-*`, `usage`, `invite`, `redeem`, `approvals`, `approve`,
  `deny`, `auto-approve` (phần ghi config đã test thuần, phần đọc
  server chưa), `fs *`, `raw`, `download`, `files`, `chat` tương tác.
- Noise handshake thật với server (cipher suite negotiation, header WS
  mà `curl_cffi` gửi ngoài `impersonate` + `verify`), và TLS
  fingerprint Chrome (xem mục Khác biệt).
- `auth export` với Chrome thật (remote debugging, `agent-browser`,
  tab muse.ai, jar có `hatch_sess`).
- Update-check với tty thật (cả stdout và stderr là tty, cache quá
  24h, có bản mới) và fetch version thật từ raw `__init__.py` trên
  fork `main`; lệnh `update` check-only với install thật (checkout /
  uv / pipx / pip).
- `subscription_action_id` mặc định còn sống hay đã cũ; cơ chế quét
  bundle có tìm được id mới không.
- Hành vi của 231 route còn lại ngoài các route đã dùng (258 tổng trừ 27 route xuất hiện dạng literal trong code): chỉ có
  method/http/path/service từ `routes.json`, không có body schema hay
  response schema (server không document, code Python cũng không dùng).
- Shape đầy đủ của mọi event `chat.subscribe` (delta, widget, task,
  approvals) ngoài những gì `wait.py` và gateway spec mô tả; đặc biệt
  shape `message.assistant` trong history so với trong live event có
  thể khác nhau.
- Ngưỡng byte server bắt đầu split response thành nhiều
  `NoiseTransportFrame`, và server có gửi chunk out-of-order thật hay
  không (code chỉ phòng thủ bằng sort theo index).
- `fs` end-to-end: norm path với server thật, chunk 4 MiB với file
  lớn, `fs cat` binary, `fs edit` với editor thật, `fs get`/`put`
  đệ quy.
- `send` end-to-end: delivery ack, `--download` sau reply, exit 5 khi
  không ack, `wait` timeout 124 và needs-input/approval với server thật.
- Render `--help` thật của bản gốc để đối chiếu từng dòng (bản gốc
  không chạy được ở máy khảo sát do thiếu `curl_cffi`; đối chiếu hiện
  tại dựa trên code parser, không phải output đã chạy).

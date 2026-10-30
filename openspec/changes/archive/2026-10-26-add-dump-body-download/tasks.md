# 任务清单：add-dump-body-download

## 状态：ARCHIVED

## 任务

### 后端 — dump 领域模块（`src/common/dump.rs`）

- [x] **T1. 新建 `src/common/dump.rs` 并迁移 dump 领域逻辑（目录参数化）**（新建 `src/common/dump.rs`、`src/common/mod.rs`）
  定义常量 `DUMP_DIR`、`MAX_DUMP_FILES: usize = 200`、`MAX_ARCHIVE_BYTES: u64 = 128 MiB`；类型 `DumpFileMeta { name, size_bytes, created_at_ms }`（`derive(Debug, Serialize)` + `#[serde(rename_all = "camelCase")]`）；纯函数 `dump_file_name(ts_millis, attempt)`、`is_valid_dump_file_name(name)`（字节扫描白名单，拒绝 `/`、`\`、NUL、非 ASCII、超长名）、`dump_file_timestamp_ms(name) -> Option<u64>`；**参数化文件系统函数** `list_dump_files_in(dir) -> Vec<DumpFileMeta>`、`read_dump_file_in(dir, name) -> Option<Vec<u8>>`、`write_dump_body_in(dir, body, ts_millis) -> io::Result<PathBuf>`、`evict_for_incoming_in(dir)`；生产包装 `list_dump_files()` 指向 `DUMP_DIR`。在 `common/mod.rs` 注册 `pub mod dump;`。
  **`evict_for_incoming_in` 语义**：保留最新 `MAX_DUMP_FILES - 1` 个，为「即将写入的新文件」腾位（等价现状 `post_messages.rs:400-409` 的写前腾位逻辑）——**不得实现为通用 keep-newest-200**，否则 `len == 200` 时会删 0 个、写入后短暂出现 201 个，破坏上限不变量。
  验证：`cargo test dump_file dump_` 通过（新测试按 `dump_` 前缀命名）；单测用 `temp_dir + tag + pid`（对齐 `src/admin/handlers.rs:637` 先例）注入临时目录，断言：`is_valid_dump_file_name` 对 `body-1700000000000.json` / `body-1700000000000-3.json` 为 true，对 `../config.json`、`body-x.json`、`notes.txt`、`sub/body-1.json`、含 `\`、NUL、非 ASCII、超长名者为 false；`dump_file_timestamp_ms("body-1700000000000-3.json") == Some(1700000000000)`；`write_dump_body_in` 落盘后文件权限为 0600（unix）；**`evict_for_incoming_in` 在目录已有 200 个文件时删至 199**（断言删除数量与「删最旧」取舍正确）。

- [x] **T2. 切换 `post_messages.rs` 到 `common/dump.rs` 并统一清理谓词**（`src/anthropic/handlers/post_messages.rs`）
  `dump_kiro_request_body` 改为调用 `write_dump_body_in(Path::new(DUMP_DIR), body, ts)`（保留现有 warning 日志）；删除已迁移的局部 `const MAX_DUMP_FILES`、目录字面量、命名拼接、清理遍历；**清理谓词由 `extension() == "json"` 改为 `is_valid_dump_file_name`**；`set_kiro_body_dump_enabled` / `kiro_body_dump_enabled` / AtomicBool 保持在原处不动；原 dump 相关单测迁至 `common/dump.rs`。
  验证：`cargo test kiro_body_dump` 通过（开关单测不回归）；新增单测断言在 200 个 `body-*` 文件 + 1 个 `notes.json` 的临时目录中触发落盘后，被删除的是最早的 `body-*` 而非 `notes.json`；**断言落盘后目录内 `body-*` 文件数量恒 ≤ 200**（上限不变量，防 `evict` 语义退化）；`cargo check` 后 `wc -l src/anthropic/handlers/post_messages.rs` 行数较改动前下降。

### 后端 — ZIP 打包

- [x] **T3. 实现 STORED ZIP 字节产出 helper**（新建 `src/admin/zip.rs`）
  导出 `local_header(name, crc, size) -> Vec<u8>`（30B + 文件名）、`central_entry(&CentralEntry) -> Vec<u8>`（46B + 文件名）、`eocd(count, cd_size, cd_offset) -> Vec<u8>`（22B）、`struct CentralEntry { name, crc, size, local_header_offset }`；CRC-32 复用 `crate::kiro::parser::crc::crc32`；`version needed = 20`、`method = 0`、`gp flag = 0`、`mod time/date = 0`、文件名取 basename。**注释写明「不实现 ZIP64，上限须 < 4 GiB」**。批量整包函数标注 `#[cfg(test)]`。
  验证：单测覆盖 (a) 0 文件（仅 EOCD），(b) 单文件，(c) 多文件；断言本地头 CRC 等于该条目数据的 `crc32()`、长度字段等于数据长度；**手工按规范重算** EOCD 的 total entries / cd_size / cd_offset 并与 helper 产出比对（不依赖实现自身产出的变量）；断言中央目录每条的文件名与 `local_header_offset` 正确；断言 `MAX_ARCHIVE_BYTES < 4 GiB`（ZIP64 不变量）。

### 后端 — 三个端点

- [x] **T4. 实现 dump 文件列表 handler**（新建 `src/admin/dump_handler.rs`）
  `GET /api/admin/dump-kiro-body/files` → 200 + `DumpFilesResponse { files, totalBytes, maxFiles, maxArchiveBytes }`；目录不存在或无匹配文件时返回空 `files` 与 `totalBytes = 0`（不报错）；`maxFiles` / `maxArchiveBytes` 取自 `common::dump` 常量。
  验证：**直接 `await` handler 函数**（无状态，不依赖 `AdminState`）做单测：临时目录内 2 个合法文件 + 1 个 `notes.json` → 断言 `files` 含 2 项、按名排序、`totalBytes` 为两者之和、`maxFiles == 200`、`maxArchiveBytes == 128 MiB`；空目录 → 空数组；响应 JSON 字段名为 camelCase。

- [x] **T5. 新增 Admin 响应类型**（`src/admin/types.rs`）
  定义 `DumpFilesResponse`（引用 `crate::common::dump::DumpFileMeta`，含 `max_archive_bytes`），`derive(Debug, Serialize)` + `#[serde(rename_all = "camelCase")]`。**不新增与 `DumpFileMeta` 重复的类型**。
  验证：`cargo check` 通过；单测断言序列化后字段名为 `files` / `totalBytes` / `maxFiles` / `maxArchiveBytes`，且 `files[0]` 为 `name` / `sizeBytes` / `createdAtMs`。

- [x] **T6. 实现单文件下载 handler 与路径穿越防护**（`src/admin/dump_handler.rs`）
  `GET /api/admin/dump-kiro-body/files/{name}`：先经 `is_valid_dump_file_name` 校验（不合法 → 400），再 `read_dump_file_in(DUMP_DIR, name)`（不存在 → 404）；成功返回原始字节 + `Content-Type: application/json` + `Content-Disposition: attachment; filename="<name>"` + `X-Content-Type-Options: nosniff`；错误响应不含绝对路径。
  验证：**直接 `await` handler + 构造 `Path(name)`** 单测：可达恶意输入（`../config.json`、`%2e%2e%2fconfig.json`、`body-x.json`、`notes.txt`）→ 400；合法名但文件不存在 → 404；合法名且存在 → 响应体字节与磁盘一致、header 三项正确。防御深度输入（`sub/body-1.json`、`body-1.json\0`）在纯函数层断言被拒（**测试注释写明 HTTP 层不可达**）。

- [x] **T7. 实现全量打包下载 handler（流式）**（`src/admin/dump_handler.rs`）
  `GET /api/admin/dump-kiro-body/archive`：核心 `pack_archive(files, max_bytes)`；**响应体开始前先列表预检，无可纳入文件 → 404**；用 `axum::body::Body::from_stream` 逐文件产出 chunk（读文件 → 算 CRC/长度 → 写本地头 → 写数据 → 释放），生成器内维护 `Vec<CentralEntry>` 与运行偏移，末尾输出中央目录 + EOCD；`Content-Type: application/zip` + `Content-Disposition: attachment; filename="kiro-body-dump-<YYYYMMDD-HHMMSS>.zip"`；**从最新到最旧累积**，超限时**整文件跳过**；发生截断或流中读取失败时在所有数据之后、中央目录之前追加 `README.txt` 条目（**计入 total entries**）；流中读取失败不中断流。
  验证：单测断言 (a) 空目录 → 404，(b) 多文件流：把 chunk 拼接为 `Vec<u8>` 后与 `#[cfg(test)]` 批式整包产物**逐字节比对**（两者共用同一组 helper），并校验拼接结果的数据段与源文件逐字节一致，(c) 传小 `max_bytes` → 截断分支生效、`README.txt` 存在且计入 entries、保留的是较新文件，(d) 模拟流中文件被删除 → 跳过并记录、`README.txt` 存在且计入 entries、归档字节仍可被批式解析逻辑完整还原，(e) 断言 `Content-Disposition` 文件名前缀为 `kiro-body-dump-`。

- [x] **T8. 挂载新端点**（`src/admin/router.rs`、`src/admin/mod.rs`）
  三个路由加入 `protected` 子路由分组，复用 `admin_auth_middleware`，不新写鉴权代码；`mod dump_handler;` 注册。
  验证：`cargo check` + `cargo clippy` clean。**不做 router 级 401 单测**（已核实 `ServiceExt`/`oneshot`/`tower::` 零命中，`tower` 非直接依赖、无 `[dev-dependencies]`、无 `src/lib.rs`，引入即违反「不新增外部 crate」）——认证由既有 `admin_auth_middleware`（本次不改动）+ T13 手工 `curl` 覆盖。

### 前端 — admin-ui

- [x] **T9. 新增 dump 下载 API 调用层**（`web-ui/admin-ui/src/api/credentials.ts` + `web-ui/admin-ui/src/types/api.ts`）
  新增 `DumpFileMeta` / `DumpFilesResponse` 类型（字段 `name` / `sizeBytes` / `createdAtMs`、`files` / `totalBytes` / `maxFiles` / `maxArchiveBytes`）与 `getDumpFiles()`（axios，走既有 `x-api-key` 拦截器）。
  验证：`pnpm build` 通过（TypeScript 编译无错）。

- [x] **T10. 新增 dump 文件列表弹窗组件**（新建 `web-ui/admin-ui/src/components/dump-files-dialog.tsx`）
  复用 `components/ui/dialog.tsx`；表格展示文件名 / 大小 / 本地时间，最新在前；每行「下载」按钮与底部「打包下载全部 (ZIP)」按钮均用 `fetch` + `x-api-key` 头 + `Blob` + `<a download>`（绕过 axios，避免 JSON 解析破坏二进制），带 loading 态与 `AbortSignal.timeout`；`totalBytes > maxArchiveBytes` 时显示截断提示（**阈值取自响应，不硬编码**）；展示 `maxFiles` 语义；空目录空态文案；404 时 toast「文件已被清理，请刷新列表」。
  验证：`pnpm build` 通过；本地开 dump 开关产生若干请求后，点按钮能下载到文件、ZIP 可解压且内容与原文件一致。

- [x] **T11. 设置面板接入下载入口并验证双端布局**（`web-ui/admin-ui/src/components/settings-panel.tsx`）
  在「Kiro 请求体落盘取证」`Row` 内新增「下载」按钮（`Button`，与开关并排），点击打开 T10 弹窗；窄屏下该 `Row` 不得溢出或挤压开关。
  验证：`pnpm build` 通过；该行出现入口按钮，开关关闭时按钮仍可用（历史文件仍可下载）；**按 <1024px 与 ≥1024px 两档视口实际验证布局**（项目硬约束）。

- [x] **T12. 补充中英文 i18n 文案**（`web-ui/admin-ui/src/i18n/locales/zh.json` + `en.json`）
  新增「下载」「打包下载全部」「暂无落盘文件」、列头、截断提示、404 提示等键，中英文键集完全对齐。
  验证：构建后切换语言，两语言下无缺失键回退（无 `dumpFiles.xxx` 原样显示）。

### 收尾

- [x] **T13. 全量验证**：`cargo fmt` + `cargo clippy`（clean）+ `cargo test`（无失败）+ `cd web-ui/admin-ui && pnpm build`。附**手工**验证（自动化无法覆盖部分）：
  1. 本地/容器启动服务，开 dump 开关 → 触发一次 `/v1/messages` → 网页端列表出现该文件 → 单个下载与 ZIP 下载均可读
  2. 下载的 ZIP 用系统 `unzip -t` 校验完整性（单测只能证明自洽，无法证明外部工具可识别）
  3. `curl` 不带 `x-api-key` 请求三个端点，确认均返回 401
  4. 设置面板该行按 <1024px 与 ≥1024px 两档视口验证布局
  5. 确认 `Cargo.toml` 的 `[dependencies]` 与 `[dev-dependencies]` 均无新增项

## 验收标准

- [x] Admin UI 设置面板「Kiro 请求体落盘取证」行内存在下载入口，点击后能看到 dump 目录内的文件列表（文件名、大小、时间）
- [x] 列表中可下载单个 dump 文件，文件内容与服务器上原始文件逐字节一致
- [x] 可一键打包下载全部 dump 文件为 ZIP，且该 ZIP 能被 `unzip -t` 与系统解压工具正常解压，解压内容与源文件逐字节一致
- [x] dump 目录不存在或为空时，列表显示空态而非报错；打包时返回 404 并在前端给出明确提示
- [x] 路径穿越尝试（`../`、`/`、URL 编码、NUL、非 ASCII、超长名）一律被拒，无法读取 dump 目录外的文件，错误响应不含服务器绝对路径
- [x] 三个新端点挂载在既有 admin 认证中间件下；`curl` 不带 `x-api-key` 时返回 401（手工验证，见 T13）
- [x] 现有 dump 落盘触发时机、文件命名、目录、0600 权限、200 上限**数值**均与变更前一致
- [x] 清理谓词已与列表谓词统一：手工放入的 `notes.json` 不占用 200 名额、不被删除、也不出现在列表中
- [x] 归档截断时按「从最新到最旧」累积，保留最近文件；`README.txt` 计入归档条目总数且位于所有数据之后、中央目录之前
- [x] 流式输出期间文件被清理时跳过并记录，归档仍可正常解压（不产生损坏条目）
- [x] 归档生成器的内存持有量为「当前文件字节 + 中央目录元数据」：**数据缓冲 O(1)，不累积已发送文件的数据字节**（已在 `test_generator_state_does_not_accumulate_sent_data` 中断言生成器状态字段：`pending` 严格递减、`central`/`central_len` 仅按条数累积）
- [x] `MAX_ARCHIVE_BYTES < 4 GiB` 的不变量由**编译期守卫**保证（`src/common/dump.rs:31` 的 `const _: () = assert!(MAX_ARCHIVE_BYTES < 4 * 1024 * 1024 * 1024);`；单测形式因常量折叠被 clippy `assertions_on_constants` 拒绝，故改为编译期形式，等价强度高于单测）
- [x] 未引入任何新的外部 crate（`[dependencies]` 与 `[dev-dependencies]` 均无新增项）
- [x] `cargo fmt` 与 `cargo clippy` clean，`cargo test` 全部通过，admin-ui `pnpm build` 成功
- [x] 设置面板新增按钮在 <1024px 与 ≥1024px 两档视口下布局正常，不影响现有开关交互
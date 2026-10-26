# 技术方案：add-dump-body-download

## 上下文

本变更属 `cache-observability` capability 的延伸。该能力已定义 dump 落盘行为（`Kiro 请求体落盘开关配置化`），但缺少取件通道 —— 现有 Admin API 仅有 `/api/admin/logs/*`（tracing ring buffer）三端点，与文件系统真值的 dump 目录无关。

相关现状（已读取源码确认）：

- `src/anthropic/handlers/post_messages.rs:382-447` —— dump 实现：函数局部 `const MAX_DUMP_FILES: usize = 200`、目录字面量 `Path::new("app/config/kiro-body-dump")`（**相对路径**，真实位置由进程 CWD 决定）、清理谓词 `p.extension().is_some_and(|x| x == "json")`、命名 `body-{ts}.json`（冲突时 `body-{ts}-{attempt}.json`）、`mode(0o600)`
- 同文件共 **471 行**（含开关函数与测试），再加 dump 领域逻辑会逼近 800 行文件上限
- `src/model/geo.rs:28` —— `pub fn new(data_dir: &Path)`，目录参数化的既有先例
- `src/admin/handlers.rs:637` —— `temp_config_dir(tag)` = 系统临时目录 + tag + 进程 pid，临时目录隔离的既有先例
- `src/admin/log_handler.rs:140` —— `download_logs` 的响应头范式（`attachment` + `from_str` 失败回退）
- `src/admin/router.rs:126` —— `/logs/download` 挂在 `protected` 子路由（`admin_auth_middleware` 已保护），端点自身不再重复鉴权
- `src/kiro/parser/crc.rs:8-14` —— `pub fn crc32` + `CRC_32_ISO_HDLC`，正是 ZIP 规范要求的算法
- `Cargo.toml` / `Cargo.lock` —— **无 flate2 / zip / miniz_oxide 等任何压缩库，无 `regex`**；无 `[dev-dependencies]`；无 `src/lib.rs`（bin-only）
- `src/` 内 `ServiceExt` / `oneshot` / `tower::` **零命中**
- `Body::from_stream` 已有先例（`src/anthropic/handlers/stream.rs:136`、`src/openai/handlers.rs:381`、`src/anthropic/websearch.rs:488`）
- `web-ui/admin-ui/src/components/log-viewer-page.tsx:251` —— 已有「fetch + Blob + `<a download>`」下载范式，可对齐
- **实测容量**：`app/config/kiro-body-dump/` 现有 4 文件各 167~172 KB → 200 文件峰值约 **34 MB**

## 目标 / 非目标

**目标：** 在 Admin UI 内提供 dump 文件的浏览与下载（单个 + 打包），消除 SSH/`docker cp` 取件成本；不引入新 crate、不改变落盘行为、数据缓冲为 O(1)。

**非目标：** dump 内容解析 / 高亮 / diff；服务端删除；压缩（ZIP 用 STORED）；ZIP64；改动日志端点与 ring buffer 机制。

## 决策

### 决策 1：dump 领域逻辑独立为 `src/common/dump.rs`，并做目录参数化

**选择：**

```rust
// src/common/dump.rs（与已有 common/fs.rs、common/auth.rs 并列）
pub const DUMP_DIR: &str = "app/config/kiro-body-dump";
pub const MAX_DUMP_FILES: usize = 200;
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;

pub struct DumpFileMeta { pub name: String, pub size_bytes: u64, pub created_at_ms: u64 }

pub fn dump_file_name(ts_millis: u64, attempt: usize) -> String;
pub fn is_valid_dump_file_name(name: &str) -> bool;
pub fn dump_file_timestamp_ms(name: &str) -> Option<u64>;
pub fn list_dump_files_in(dir: &Path) -> Vec<DumpFileMeta>;
pub fn read_dump_file_in(dir: &Path, name: &str) -> Option<Vec<u8>>;
pub fn write_dump_body_in(dir: &Path, body: &str, ts_millis: u64) -> std::io::Result<PathBuf>;
pub fn evict_for_incoming_in(dir: &Path);   // 保留 MAX_DUMP_FILES - 1，为即将写入的文件腾位
pub fn list_dump_files() -> Vec<DumpFileMeta>;   // 生产包装，指向 DUMP_DIR
```

**`evict_for_incoming_in` 的语义锁定（保持不变量，勿实现为通用 keep-N）：** 现状是「**写前腾位**」—— `entries.len() >= MAX_DUMP_FILES` 时删除 `len - MAX_DUMP_FILES + 1` 个（删至 199）再写入，故落盘后目录内 `body-*` 数量**恒 ≤ 200**（`post_messages.rs:400-409`）。若改为通用的「保留最新 200」，在 `len == 200` 时删 0 个，写入后短暂出现 201 个，与「200 上限数值不变」的承诺相悖。故函数名与语义都锁定为「为即将到来的写入腾位」，并由单测断言该不变量（T2 验证）。

**理由（三选一，此处选模块 + 参数化）：**

1. **模块位置**：放进 `post_messages.rs`（原方案）会让该文件从 471 行继续膨胀至逼近 800 行上限（违反文件规模约定），且迫使 `src/admin/dump_handler.rs` 经 `handlers/mod.rs` → `anthropic/mod.rs` 两级重导出**反向依赖 anthropic handler 的内部实现**。独立成 `src/common/dump.rs` 让 admin 与 anthropic 平级引用同一模块，无交叉依赖。选 `common/` 而非 `anthropic/handlers/dump.rs`：该逻辑不含任何 Anthropic 协议语义（是通用文件管理），且 admin 侧使用不需要跨模块层级。
2. **目录参数化**：**这是让单测可执行的前提。** 无参签名只能读相对 CWD 的 `app/config/kiro-body-dump`，无法指向测试目录，T4/T5/T6 的「用临时目录构造文件」验证条款将全部无法落地。`src/model/geo.rs:28` 与 `src/admin/handlers.rs:637` 两个先例已确立此模式。**不使用** `std::env::set_current_dir`：测试并行执行会互相干扰，且项目无此先例。
3. **生产包装**：保留无参 `list_dump_files()` 指向 `DUMP_DIR`，避免 admin handler 每处都拼路径。

**`post_messages.rs` 的改动边界：** 只把 `dump_kiro_request_body` 的实现体改为调用 `write_dump_body_in`；`set_kiro_body_dump_enabled` / `kiro_body_dump_enabled` / `KIRO_BODY_DUMP_ENABLED` 两个开关函数与 AtomicBool **保持在原处不动**（它们是 HTTP handler 层的运行时开关，非 dump 领域逻辑，移动会扩大改动面与 CR 面积）。

**注意 `src/common/dump.rs` 与 `src/admin/zip.rs` 的职责划分：** 前者管「dump 文件系统语义」，后者管「ZIP 字节格式」，两者不互相依赖。

### 决策 2：手写 STORED ZIP，不引入压缩 crate

**选择：** 按 ZIP 规范手写 STORED（method 0）布局，CRC-32 复用 `crate::kiro::parser::crc::crc32`。

**理由：** dump 内容是 JSON 文本，deflate 可省约 80% 体积，收益显著 —— 但项目约定「不引入新外部 crate」，且 `flate2` 会带 `miniz_oxide`、`libz-sys` 等传递依赖，与约定冲突。STORED 零依赖、可完全单测。

**代价：** 归档体积 ≈ 源文件总和。实测峰值 34 MB 量级，可接受（用决策 3 的流式输出进一步约束内存）。

**布局与关键字段：**

```
本地文件头 (30B) + 文件名 | 文件数据 |  ... × N
中央目录记录 (46B) + 文件名 | ...          × N
EOCD (22B)
```

| 字段 | 取值 |
|---|---|
| version needed to extract | 20 |
| general purpose bit flag | 0 |
| compression method | 0（STORED） |
| last mod file time / date | 0（避免 MS-DOS 时间转换；文件名已含毫秒时间戳） |
| crc-32 | `CRC_32_ISO_HDLC` |
| compressed / uncompressed size | 相等 |
| file name | basename，无路径分隔符 |
| EOCD: total entries / central dir size / central dir offset | 与字节位置严格一致 |

**不实现 ZIP64 的依赖关系（须写入 `zip.rs` 注释）：** 中央目录偏移用 u32 表示，跨 4 GiB 会溢出。`MAX_ARCHIVE_BYTES` 必须小于 4 GiB 才能保证安全；单测断言该常量 < 4 GiB，防止后续调高上限时静默引入溢出。

### 决策 3：流式响应，且与批量构造共用同一组 helper

**选择：** 生产路径只有一条 —— `axum::body::Body::from_stream` 逐文件产出 chunk。`zip.rs` 只导出纯字节 helper：

```rust
fn local_header(name: &str, crc: u32, size: u32) -> Vec<u8>   // 30B + name
fn central_entry(e: &CentralEntry) -> Vec<u8>                // 46B + name
fn eocd(count: u16, cd_size: u32, cd_offset: u32) -> Vec<u8> // 22B
struct CentralEntry { name: String, crc: u32, size: u32, local_header_offset: u32 }
```

批量整包函数标注 `#[cfg(test)]`，仅用于「一次性构造完整归档做字节比对」（仓库规则禁止保留无调用点的生产代码）。

**理由：** 完整构造会让服务端内存峰值 = 目录总和，与「代理服务不该被单个管理请求拖垮」冲突。helper 共用则避免批式 / 流式两套 ZIP 逻辑各自演进而分叉。

**内存持有量（可断言的表述）：** 生成器状态 = 「当前单个文件的字节」+ `Vec<CentralEntry>`（200 × 约 30B ≈ 6 KB）。**数据缓冲 O(1)，元数据随文件数线性。**

**流控制：** 用 `futures::stream::unfold` / 手写 `Stream`（`futures` 已是依赖），不引入 `async-stream`。

### 决策 4：归档总量上限 128 MiB，定位为防御性上限

**选择：** 核心函数 `pack_archive(files, max_bytes)`，生产传 `MAX_ARCHIVE_BYTES = 128 MiB`，测试传小值以覆盖截断分支。

**理由与诚实定位：** 实测 200 文件峰值约 **34 MB**，故任何常规阈值在 200 文件上限下都**不会在生产触发截断**（原方案写 512 MiB，与实测相差 15 倍，且风险表「数百 MB」的估算错误）。阈值取 128 MiB ≈ 4× 实测峰值，为单文件体积增长留裕度，但**其真实作用是防御性上限而非预期路径**：截断分支只能通过参数注入在单测中覆盖。

**排序与截断方向：** 列表按文件名排序（文件名内为 **13 位定长毫秒时间戳**，故字典序 = 时间序；数值本身不补零，但定长保证 Comparable）；截断时**从最新到最旧累积**，保留最近的取证数据。粒度为**整文件** —— 某文件加入会超限则整个跳过。

### 决策 5：端点挂载在现有 `protected` 子路由，认证不做 router 级测试

**选择：** 三个端点全部加在 `create_admin_router` 的 `protected` 分组内。

**理由：** 复用 `admin_auth_middleware`（`x-api-key` 头认证），与 `/logs/download` 同级，不新写鉴权代码、不扩大未认证面。**不**放在 `log_handler.rs`（该文件职责是 ring buffer），新建 `dump_handler.rs` 保持职责单一。

**认证测试手段（明确取舍）：** 已核实 `src/` 内 `ServiceExt`/`oneshot`/`tower::` 零命中，`tower` 非直接依赖（仅 `tower-http`），无 `[dev-dependencies]`，无 `src/lib.rs`（bin-only，无法写 `tests/` 集成测试）。故**不为测试引入 `tower`**。认证行为依赖既有 `admin_auth_middleware`（本次不改动）+ T13 手工 `curl` 验证。

**但 handler 本身可直接单测：** dump handler 不依赖 `AdminState`（无状态），故可在单测中直接 `await` handler 函数并构造 `Path` 参数，覆盖 200 / 400 / 404 与响应头 —— 这比只测底层纯函数更强。T4 / T5 / T6 均采用此方式。

### 决策 6：前端下载绕过 axios，用 fetch + `x-api-key` 头

**选择：** 与 `log-viewer-page.tsx` 的下载范式对齐 —— 直接 `fetch` + `Blob` + `<a download>`。

**理由：** axios 默认 `responseType: 'json'`，二进制 ZIP 响应会被解析破坏；且下载需处理 `Content-Disposition` 文件名与 `AbortSignal.timeout`。沿用已验证范式，避免新引入下载工具函数。

**鉴权头：** 用 `x-api-key` 头而非 `api_key` 查询参数 —— 后者会进入反代 access log（`log_handler.rs` 注释已明确此风险）。

**ZIP 前端策略：** fetch → `blob()` → `URL.createObjectURL` → `<a download>`。**不能**用 `window.open(url)` 直连（无法带自定义头）。

### 决策 7：类型归属 —— 只保留一个领域类型

**选择：** `DumpFileMeta` 定义在 `src/common/dump.rs` 并**直接 `derive(Serialize)` + `#[serde(rename_all = "camelCase")]`**，由 `DumpFilesResponse` 引用。**删除原方案中与之逐字段重复的 `DumpFileItem`**。

```rust
// src/admin/types.rs —— 仅响应包裹类型，含新增的 maxArchiveBytes
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpFilesResponse {
    pub files: Vec<DumpFileMeta>,
    pub total_bytes: u64,
    pub max_files: usize,
    pub max_archive_bytes: u64,   // 前端据此判断是否提示截断，不硬编码阈值
}
```

**理由：** 原 `DumpFileMeta` 与 `DumpFileItem` 三个字段完全相同，且 T2 要求「一对一构造」，属纯粹的重复定义。合并后只有一个类型、一处 serde 配置。`max_archive_bytes` 随列表下发，使前端免于硬编码 `MAX_ARCHIVE_BYTES`。

### 决策 8：统一滚动清理与列表的过滤谓词（行为变更，已获用户确认）

**现状分叉：** 清理用 `p.extension().is_some_and(|x| x == "json")` 计数保留 200 个；列表若用严格校验，则手工放入的 `notes.json` 会占 200 名额、可能挤掉最早的 `body-*`，却永不出现在列表中 —— 用户看到"目录有文件、列表为空"却无法解释。

**选择：** 清理改为先用 `is_valid_dump_file_name` 过滤再计数；列表用同一函数。两处同源。

**代价：** 本次唯一的既有行为变更（此前 `notes.json` 会被计入并可能被删）。属修正而非回归：旧行为会误删用户手工放入的 `.json`。已在 proposal 的「在范围内」「预期影响」「风险」三处显式标注。

### 决策 9：`createdAtMs` 取文件名时间戳，不取文件系统时间

**选择：** 从文件名解析（`body-<ts>[-<n>].json`，后缀不影响 `ts`）。

**理由：** (a) 与滚动清理的排序口径一致（清理按文件名排序 ≡ 时间序）；(b) 文件被 `cp` / `touch` / 跨文件系统拷贝后 mtime 失真，而文件名时间戳是落盘真值；(c) 同一毫秒多请求由 `-N` 后缀区分，按名有序仍自洽。

### 决策 10：流式期间文件缺失 / 无文件可打包的处理

**选择：**
- 开始流式输出**之前**先列表预检；无可纳入文件 → 404（状态码在响应体开始前确定）
- 流式过程中读取失败（被滚动清理并发删除、IO 错误）→ **跳过并在 `README.txt` 记录其名**，不中断流

**理由：** 200 上限的滚动清理与新请求落盘可并发发生于流式输出期间，这是真实竞态。一旦响应头与部分 body 已发出，HTTP 层无法再改状态码，故「跳过 + 记录」是唯一可行且可观测的降级方式。

**`README.txt` 的完整语义（原方案此处有缺口，现补齐）：**
- **生成条件**：发生截断 **或** 发生流中跳过时生成；正常情况下不存在
- **物理位置**：固定为**所有 dump 数据之后、中央目录之前**（内容需遍历结束后才能确定，故只能在流末段输出）
- **计数**：作为普通条目**计入中央目录与 EOCD 的 total entries**
- **文件名**：`README.txt`（与其他条目的 basename 同级，无路径前缀）

### 决策 11：路径校验不引入 `regex`

**选择：** `is_valid_dump_file_name` 手写字节扫描：前缀 `body-`、1+ 个 ASCII 数字、可选 `-` + 1+ 个数字、后缀 `.json`、**全串长度上限**、显式拒绝 `/`、`\`、NUL、非 ASCII。

**理由：** `Cargo.toml` 无 `regex` 依赖（已核实），引入即违反约定。手写约 20 行、无回溯、可比 `regex` 更严格（显式白名单字节），且天然满足「拒绝分隔符」的硬要求。

**可达性说明（测试预期须准确）：** 经 HTTP 层时，`{name}` 是单个路径段（不含 `/`），且 NUL 不能出现在合法 URI 中，故 `sub/body-1.json`、`body-1.json\0` 这类输入**在 HTTP 层不可达**。对它们的测试属**防御深度**（纯函数层），而非可达路径 —— 测试注释须写明，避免读者误以为可从外部触发。

## 风险 / 权衡

| 风险 | 应对 |
|---|---|
| 手写 ZIP 字段/偏移错误导致解压工具拒绝 | helper 级单测逐字段断言 + 手工按规范重算 EOCD 的 entries / cd_size / cd_offset 并比对（不依赖实现自身产出的变量）；覆盖 0 / 1 / 多文件；T13 手工 `unzip -t` 交叉验证（自动化测试无法证明外部工具能识别产物） |
| **阈值在生产不可达** | 明确定位为防御上限（决策 4），已在 proposal 风险表显式声明；截断分支仅靠参数注入在单测覆盖 |
| 不实现 ZIP64 的隐含约束 | `MAX_ARCHIVE_BYTES` < 4 GiB，单测断言该不变量 + `zip.rs` 注释写明「调高上限前先评估 ZIP64」 |
| 路径穿越 | `is_valid_dump_file_name` 白名单字节扫描（决策 11），先校验后拼接；测试覆盖可达与防御深度两类输入 |
| 流式实现中「先写头后算 CRC」的顺序错误 | 实现顺序固定：读文件 → 算 CRC/长度 → 写本地头 → 写数据；单测断言本地头 CRC 等于数据实际 `crc32()` |
| 决策 1 迁移触及请求热路径 | `write_dump_body_in` 为等价迁移 + 参数化，不改逻辑分支；dump 相关单测整体迁移至 `common/dump.rs` 并保持通过；CR 时重点复核 `post_messages.rs` 与 `dump.rs` diff |
| **清理谓词收紧属行为变更** | 已在 proposal 三处显式标注并获用户确认；影响仅限「手工放入的非 `body-*` 的 `.json` 不再被删除」 |
| 前端 Blob 内存占用 | 实测峰值 34 MB 量级，浏览器可承受；超 `maxArchiveBytes` 时前端提示 |
| 目录在列表与下载之间被滚动清理 | 单文件下载 404 + 前端 toast「文件已被清理，请刷新列表」，不重试 |
| 设置面板新增按钮破坏窄屏布局 | T11 按 <1024px 与 ≥1024px 两档实际视口验证（手工验证集中在 T13） |

## 迁移方案

**代码迁移：** dump 落盘实现从 `post_messages.rs` 迁至 `common/dump.rs`（含参数化改造），行为等价 —— 除清理谓词外不改任何逻辑分支。现有 dump 相关单测随之迁移。

**数据迁移：** 无需。升级前落盘的文件名同样满足新校验规则，历史文件全部可见可下载。

**产物迁移：** 前端 `dist` 需重新构建才能看到入口；后端升级后旧前端不受影响（新增端点，无既有端点变更）。

## 待决问题 / 未消解项

**用户已决策项（无待决）：** 下载形态（列表 + 单个 + ZIP）、入口位置（设置面板行内）、清理与列表谓词统一 —— 三项均在阶段 0 或后续澄清中由用户明确选定。

**审查轮次结论（如实记录，不掩盖）：** 本提案的 proposal/tasks 质量审查共执行 2 轮（流程硬上限）。**第 2 轮结论仍为 `NEEDS_REVISION`**，共列出 14 项问题（I1~I14，含 5 项阻塞）。主 agent 已按全部 14 项修正四份产物，但**该修订结果未经第 3 轮 sub-agent 审查确认**（轮次上限所致）。用户已知悉并选择「确认实施」继续。故下列条目属「已修正但未经审查复核」的残留关注点，实施阶段需自行留意：

| # | 第 2 轮问题 | 修正动作 | 残留风险 |
|---|---|---|---|
| I1 | 无参签名与「临时目录」验证条款自相矛盾 | 全部文件系统函数 `*_in(dir: &Path)` 参数化 + 生产包装 | 无 —— 修正直接消除矛盾 |
| I2 | T5「纯函数级单测」被测函数未定义 | 拆出 `read_dump_file_in`；注明 `sub/body-1.json`、NUL 属防御深度 | 无 |
| I3 | 前端阈值无来源 | `DumpFilesResponse` 增 `maxArchiveBytes` | 无 |
| I4 | 阈值数量级错（原 512 MiB vs 实测 34 MB） | 改为 128 MiB 并定位为防御上限 | 阈值在生产不可达，截断分支仅靠注入覆盖 |
| I5 | T6(b)「拼回」语义不明 | 改为「chunk 拼接后与批式产物逐字节比对」 | 无 |
| I6~I14 | 类型重复 / README 语义 / 测试命令 / changelog / 内存断言 / handler 可测 / 职责错位 / ZIP64 | 逐条吸收（见决策 7 / 10 / 11 与 tasks T1~T13） | 见下 |

**后续核验补充项（步骤 4.6 需求回溯核验发现并已修正）：**

- `evict_excess_in` → 改名 `evict_for_incoming_in` 并锁定「保留 199 腾位」语义（原签名有使不变量退化为 201 的风险），T2 补断言
- 常量名统一为 `DUMP_DIR`（原 proposal 与 design/tasks 分叉为两个名字）
- design 任务编号引用漂移修正（手工验证统一归 T13）
- 「补零毫秒时间戳」表述改为「13 位定长毫秒时间戳」（与实测命名一致）
- spec.md 补前端入口与下载交互 Requirement（原仅落在 tasks / 验收标准）

**仍待实施阶段验证的假设（非待决，但需实据确认）：**

- ZIP 产物能被外部解压工具识别 —— 自动化单测只能证明自洽，需 T13 的 `unzip -t` 手工验证
- dump 单文件体量随上游请求增大时的实际峰值 —— 当前实测 167~172 KB / 文件，`MAX_ARCHIVE_BYTES = 128 MiB` 的裕度依据此数据

## 主动超出项（经用户确认保留）

需求回溯核验（步骤 4.6 第二轮，结论 `CONFORMS`）判定以下 3 项属原始需求未显式要求的内容。用户已逐项确认保留，实施与提交时须在 commit message 中标注为「主动超出，经用户确认」。

| # | 超出内容 | 保留理由 | 若不保留的代价 |
|---|---|---|---|
| 11 | dump 领域逻辑迁出 `post_messages.rs` 至 `src/common/dump.rs` 并目录参数化 | 该文件已 471 行，追加会逼近 800 行上限；且避免 admin 经两级重导出反向依赖 anthropic handler 内部实现 | 无参签名无法注入临时目录，T1 / T4 / T6 / T7 的单测验证条款全部落不了地 |
| 12 | 归档截断上限 128 MiB、截断或流中缺失时附 `README.txt`、流式产出（`Body::from_stream`）、STORED 无压缩、ZIP64 不变量单测 | 原需求仅要求「一键 ZIP 打包」，但完整读入内存会使峰值 = 目录总和，且无任何容量保护 | 需整目录读入内存构造 ZIP；单次 GET 可长时间占用连接与内存 |
| 13 | `createdAtMs` 取文件名时间戳（而非 mtime）、中英文 i18n 键集对齐、本变更不随版本发布 | 前两项为项目既有约定（与滚动清理排序口径一致、i18n 双语言对齐）；第三项与 CLAUDE.md「打 tag 必须同步版本号与 changelog」约束一致 | 时间口径与清理排序不一致；i18n 缺键回退；发版时 changelog 与版本号不同步 |
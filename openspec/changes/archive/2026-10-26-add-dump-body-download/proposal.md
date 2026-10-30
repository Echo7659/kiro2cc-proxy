# 变更提案：add-dump-body-download

## 背景

`dumpKiroBody` 开关（config.json `dumpKiroBody` / 环境变量 `KIRO_DUMP_BODY=1`）开启后，每个发往 Kiro 的最终请求体会落盘到 `app/config/kiro-body-dump/`，文件名 `body-<毫秒时间戳>.json`，权限 0600，最多保留 200 个。

问题在于**取件路径完全缺失**：Admin 现有 `/api/admin/logs/download` 仅覆盖 tracing 日志 ring buffer，`kiro-body-dump` 目录既没有列表接口也没有下载接口。部署在服务器（Docker / systemd / `start_server.sh`）时，用户必须 SSH 登录后用 `find` 定位目录（落盘用的是相对路径，真实位置取决于进程 CWD：容器内 `/app/config/kiro-body-dump`，systemd 下 `/opt/kiro2cc-proxy/app/config/kiro-body-dump`），再手工 `scp` / `docker cp` 取回。对于"开开关 → 复现问题 → 取文件看 body"这一高频取证流程，中间隔着一次服务器登录，成本明显偏高。

本变更在 Admin UI 设置面板的「Kiro 请求体落盘取证」行内增加下载入口。

**实测数据（决定容量设计）：** 本机 `app/config/kiro-body-dump/` 现有 4 个文件，各 167~172 KB。按 200 个上限推算目录峰值约 **34 MB**，单文件量级为百 KB 级而非 MB 级。

## 目标范围

**在范围内：**
- 后端新增 3 个 Admin API 端点：dump 文件列表、单文件下载、全量打包 ZIP 下载
- 后端新增独立的 `src/admin/dump_handler.rs` 承载 HTTP handler
- **把 dump 的路径 / 命名 / 上限 / 校验 / 清单逻辑从 `post_messages.rs` 抽出为独立模块**（见技术方案 1），并**目录参数化**以便单测注入临时目录
- 统一滚动清理与列表的过滤谓词：现有清理按 `extension() == "json"` 计数，列表按严格 `body-<数字>[-<数字>].json` 校验，两者分叉会导致「目录内有文件、列表却为空」（手工放入的 `foo.json` 占 200 名额并可能挤掉最早的 `body-*`，却永不出现在列表中）——本次把清理谓词统一为同一严格校验函数
- 手写 ZIP（STORED 无压缩）打包实现，复用已有 `crc` crate 的 `CRC_32_ISO_HDLC`
- 前端 `settings-panel.tsx` 在 dump 开关行内新增「下载」按钮，点击打开文件列表弹窗（新增 `dump-files-dialog.tsx`）
- admin-ui 中英文 i18n 文案同步
- 后端单元测试覆盖：路径穿越防护、ZIP 字节布局、文件列表过滤与解析

**不在范围内：**
- 不改变 dump 的落盘触发时机、文件命名规则、文件权限、200 个上限的**数值**（仅统一其过滤谓词）
- 不新增 dump 文件的删除 / 清理入口（沿用现有自动滚动清理）
- 不新增任何 HTTP 级（router 层）测试工具依赖（不引入 `tower` 等 `[dev-dependencies]`）
- 不实现 ZIP64（上限设计保证 u32 偏移不溢出，见风险表）
- 不做 dump 内容的解析 / 高亮 / diff
- 不随本变更发布版本（`src/admin/changelog_data.rs` 版本条目的更新由后续发版变更负责；若本变更被并进某次发版，则该次发版需按项目约定同步 changelog）

## 技术方案

**1. dump 逻辑独立成模块并目录参数化**

现状：落盘逻辑在 `src/anthropic/handlers/post_messages.rs`（该文件已 471 行），若再追加路径 / 命名 / 校验 / 清单等 `pub(crate)` 项，会逼近 800 行上限，且迫使 `src/admin/dump_handler.rs` 经两级重导出反向依赖 anthropic handler 的内部实现。

**选择：** 新建 `src/common/dump.rs`（与已有 `src/common/fs.rs`、`src/common/auth.rs` 并列），承载全部 dump 领域逻辑；`post_messages.rs` 与 `dump_handler.rs` 均引用它，节点无交叉依赖。

**目录参数化（关键，否则单测无法落地）：** 所有涉及文件系统的函数都接受 `&Path`，由生产包装函数传入 `DUMP_DIR`：

```rust
// src/common/dump.rs
pub const DUMP_DIR: &str = "app/config/kiro-body-dump";
pub const MAX_DUMP_FILES: usize = 200;
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024; // 见风险表：防御上限

pub struct DumpFileMeta { pub name: String, pub size_bytes: u64, pub created_at_ms: u64 }

pub fn dump_file_name(ts_millis: u64, attempt: usize) -> String;
pub fn is_valid_dump_file_name(name: &str) -> bool;
pub fn dump_file_timestamp_ms(name: &str) -> Option<u64>;

// 全部文件系统操作参数化，供测试注入临时目录
pub fn list_dump_files_in(dir: &Path) -> Vec<DumpFileMeta>;
pub fn read_dump_file_in(dir: &Path, name: &str) -> Option<Vec<u8>>;
pub fn write_dump_body_in(dir: &Path, body: &str, ts_millis: u64) -> std::io::Result<PathBuf>;
// 语义：保留最新 MAX_DUMP_FILES - 1 个，为「即将写入的新文件」腾位（见下）
pub fn evict_for_incoming_in(dir: &Path);

// 生产无参包装（指向 DUMP_DIR）
pub fn list_dump_files() -> Vec<DumpFileMeta>;
```

**`evict_for_incoming_in` 的边界语义（保持不变量的关键）：** 现状实现是「**写前腾位**」—— `entries.len() >= 200` 时删除 `len - 200 + 1` 个（删至 199），随后再写入新文件，故落盘后目录内 `body-*` 数量**恒 ≤ 200**。若提取时改成通用的「keep newest 200」，在 `len == 200` 时会删 0 个，写入后短暂出现 201 个 —— 与「上限数值不变」的承诺相悖。因此函数名与语义都锁定为「为即将到来的写入腾位」（保留 199），并由单测断言不变量（T2）。

**先例（已核实）：** `src/model/geo.rs:28` 用 `pub fn new(data_dir: &Path)`；`src/admin/handlers.rs:637` 的 `temp_config_dir(tag)` 用「系统临时目录 + tag + pid」隔离。**不使用** `std::env::set_current_dir`（测试并行执行时会互相干扰，且项目无此先例）。

**`post_messages.rs` 的改动：** `dump_kiro_request_body` 改为调用 `write_dump_body_in`；`set_kiro_body_dump_enabled` / `kiro_body_dump_enabled` 两个开关函数**保持在原处不动**（它们是 HTTP handler 层的运行时开关，非 dump 领域逻辑）。落盘触发时机、命名、权限、上限数值均不变；**唯一行为变更是清理谓词改由 `is_valid_dump_file_name` 统一**。

**2. 三个新端点（挂载在已有 admin 认证中间件保护下）**

| 方法 | 路径 | 响应 |
|---|---|---|
| GET | `/api/admin/dump-kiro-body/files` | `{ files: [{name, sizeBytes, createdAtMs}], totalBytes, maxFiles, maxArchiveBytes }` |
| GET | `/api/admin/dump-kiro-body/files/{name}` | 单文件 `application/json` + `Content-Disposition: attachment; filename="body-<ts>.json"` |
| GET | `/api/admin/dump-kiro-body/archive` | ZIP（STORED）`application/zip` + `Content-Disposition: attachment; filename="kiro-body-dump-<YYYYMMDD-HHMMSS>.zip"` |

三者复用 `create_admin_router` 的 protected 子路由，认证沿用 `admin_auth_middleware`（`x-api-key` 头）。`maxArchiveBytes` 随列表下发，使前端无需硬编码阈值（前端不重复常量）。

**3. 手写 STORED ZIP（单一产出实现，批式与流式共用）**

零压缩依赖（项目约定不引入新 crate），ZIP 规范最简 STORED（method 0）布局：

```
[本地文件头 30B + 文件名]  [文件数据]   × N
[中央目录记录 46B + 文件名]            × N
[EOCD 22B]
```

关键字段：`version needed = 20`、`gp flag = 0`、`compression method = 0`、CRC-32 用 `crc::CRC_32_ISO_HDLC`（`src/kiro/parser/crc.rs` 同算法）、`compressed size = uncompressed size`、文件名取 basename、`mod time/date = 0`。小端序写入（`to_le_bytes()`），不依赖 `byteorder`。

**避免两套实现分叉：** `src/admin/zip.rs` 只导出纯字节产出 helper —— `local_header(...) -> Vec<u8>`、`central_entry(&CentralEntry) -> Vec<u8>`、`eocd(count, cd_size, cd_offset) -> Vec<u8>`，外加 `CentralEntry { name, crc, size, local_header_offset }`。批量整包函数标注 `#[cfg(test)]`（仅供测试构造完整归档做字节比对，生产路径只有流式；避免留下无调用点的生产死代码）。

**4. 流式响应控制内存峰值**

ZIP 端点在**响应体开始前**先做一次列表预检（无可纳入文件 → 404，状态码在此时确定），随后用 `axum::body::Body::from_stream` 逐文件产出 chunk：读文件 → 算 CRC/长度 → 写本地头 → 写数据 → 释放，生成器内维护 `Vec<CentralEntry>` 与运行偏移量，最后输出中央目录 + EOCD。内存持有量为「当前文件字节 + 中央目录元数据」——**数据缓冲为 O(1)，元数据随文件数线性**（200 × 约 30B ≈ 6 KB）。

**截断与竞态的边界语义：**
- 截断粒度是「整文件」：某文件加入后累计将超限时，整个文件跳过
- **累积方向从新到旧**（倒序遍历），截断时保留最新取证数据
- 发生截断**或**流中读取失败时，追加 `README.txt` 条目，**计入中央目录与 EOCD 的 total entries**；物理位置固定为**所有 dump 数据之后、中央目录之前**（需遍历结束才能确定内容，故只能在流末段输出）
- 流中文件被滚动清理（并发竞态）：该文件跳过并在 `README.txt` 记录其名，**不中断流**（响应头与部分 body 已发出，无法再改状态码）
- 正常情况下不生成 `README.txt`

**5. 前端入口与下载方式**

`settings-panel.tsx` 的 dump 开关行内新增「下载」按钮 → 打开 `dump-files-dialog.tsx`：
- 弹窗表格展示文件名、大小、本地时间，倒序（最新在前）
- 每行「下载」按钮 → fetch `/files/{name}` 带 `x-api-key` 头 → Blob → `<a download>`
- 弹窗底部「打包下载全部 (ZIP)」→ 同样 fetch + Blob 方式；`totalBytes` 超过响应中的 `maxArchiveBytes` 时显示截断提示（阈值来自后端，前端不硬编码）
- 空目录显示空态文案
- **布局须在移动端（<1024px）与桌面端（≥1024px）双端验证**（项目硬约束：移动适配不得影响 ≥1024px 网页端功能与 UI）

**6. 安全**

- **路径穿越防护**：`{name}` 经 `is_valid_dump_file_name` 严格校验（白名单语义 `^body-\d+(-\d+)?\.json$`，**无 `regex` 依赖**，用字节扫描实现），任何含 `/`、`\`、`..`、NUL、非 ASCII 或超长输入一律拒绝
- **响应头**：`Content-Disposition: attachment`、`X-Content-Type-Options: nosniff`
- **敏感内容**：dump 内容含完整对话与凭据信息，端点受 admin 认证保护，与日志端点同级

## 预期影响

- **落盘行为不变，清理行为有一处定向变更**：落盘触发时机、命名、路径、0600 权限、200 上限数值均不变；**唯一行为变更是清理的过滤谓词由 `extension() == "json"` 收紧为 `is_valid_dump_file_name`** —— 此后手工放入的 `foo.json` 不再占用 200 名额、也不再被本功能删除（属修正而非回归，已获用户确认）
- **代码组织变更**：`post_messages.rs` 的 dump 落盘实现迁至 `src/common/dump.rs`（等价迁移 + 参数化）；`post_messages.rs` 行数下降
- **API 面扩大**：新增 3 个只读端点，均为 Admin 认证保护，不影响任何客户端路径
- **前端产物变化**：admin-ui 需重新 `pnpm build`
- **性能**：列表端点需 `read_dir` 一次（200 文件量级）；ZIP 端点为顺序 IO + 流式传输
- **兼容性**：不改动任何部署脚本

> **主动超出项：** 本次含 3 项超出用户原始诉求字面范围的必要内容（dump 领域逻辑迁移 + 目录参数化、归档截断 / `README.txt` / 流式产出 / ZIP64 不变量、`createdAtMs` 口径 / i18n / 不发版），均经用户逐项确认保留，详见 `design.md`「主动超出项」节。

## 风险

| 风险 | 应对 |
|---|---|
| ZIP 手写实现有格式细节错误，生成文件无法被解压工具识别 | helper 级单测逐字段断言 + **手工按规范重算** EOCD 的 entries / cd_size / cd_offset 并比对（不依赖实现自身产出的变量）；覆盖 0 / 1 / 多文件；T13 手工用 `unzip -t` 交叉验证 |
| **`MAX_ARCHIVE_BYTES` 在生产不可达**：实测 200 文件峰值约 34 MB，任何常规阈值都不会触发截断 | 阈值定为 `128 MiB`（约 4× 实测峰值，为单文件体积增长留裕度），并**明确定位为防御性上限**：200 文件上限下截断在生产不可见，该分支仅通过 `pack_archive(files, max_bytes)` 注入小值在单测中覆盖。风险表原「数百 MB」估算是错的，已按实测纠正 |
| 不实现 ZIP64 的隐含约束 | `MAX_ARCHIVE_BYTES` 上限保证中央目录偏移 < 4 GiB（u32 可表示）；须在 `zip.rs` 注释中写明「调高上限前先评估 ZIP64 必要性」，单测断言上限常量 < 4 GiB |
| 落盘用相对路径，handler 若另写一份路径常量会在 Docker/systemd 下指向不同目录 | 路径、命名、上限、谓词全部集中到 `src/common/dump.rs`，两侧引用同一来源 |
| 清理谓词收紧后，此前手工放入的 `.json` 不再被清理，长期堆积 | 属预期（本功能不再管理非自身产物）；其不占用 200 名额，不影响 `body-*` 的滚动保留 |
| 路径穿越导致读取 dump 目录外文件 | 白名单字节校验 + 拒绝分隔符 + 只接受基名；单测覆盖 `../`、绝对路径、URL 编码、NUL、超长名、非 ASCII |
| 流式输出期间文件被滚动清理，部分 body 已发出无法改状态码 | 跳过 + `README.txt` 记录，不中断流；开始流式前先做列表预检，无文件可纳入时 404 |
| 移动 dump 逻辑改动触及请求热路径 | `write_dump_body_in` 为等价迁移 + 参数化，不改逻辑分支；dump 相关单测全部迁移到新模块并保持通过；CR 时重点复核 `post_messages.rs` 与 `dump.rs` 的 diff |
| 前端 Blob 占用内存 | 实测峰值 34 MB 量级，浏览器可承受；后端流式发送；超 `maxArchiveBytes` 时前端提示 |
| ZIP 无压缩，体积等于源文件总和 | 实测峰值 34 MB，可接受；前端按钮带 loading 态与 `AbortSignal.timeout` |
| 新增按钮破坏设置面板窄屏布局 | 双端（<1024px / ≥1024px）布局验证写入验收标准 |
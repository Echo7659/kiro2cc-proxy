# spec 增量：add-cache-observability

## Purpose
为管理员提供缓存命中率聚合可观测能力（按日统计 input/cacheRead），并将 Kiro 请求体落盘与日志文件持久化从环境变量/临时手段升级为配置项，便于运维排查。

## ADDED Requirements

### Requirement: 每日用量聚合携带缓存命中字段
系统 SHALL 在 `GET /api/admin/usage/daily` 的每日聚合结果（DailySummary）与凭证维度聚合结果（CredentialDaySummary）中包含当日累计 input tokens 与 cache read tokens 字段。

#### Scenario: 聚合字段数值正确
- **WHEN** 某日存在用量记录（input=1000, cacheRead=800）与（input=500, cacheRead=0）各一条
- **THEN** 该日 `totalInputTokens` 为 1500，`totalCacheReadTokens` 为 800
- **AND** cacheRead 缺失（None）的记录按 0 计入，不影响其他字段

#### Scenario: 空数据日
- **WHEN** 某日无任何用量记录
- **THEN** 该聚合记录的两个新字段为 0，不影响现有字段行为

### Requirement: Kiro 请求体落盘开关配置化
系统 SHALL 支持 `config.json` 中的 `dump_kiro_body` 布尔配置项（默认 false）控制 Kiro 请求体落盘行为，且保留 `KIRO_DUMP_BODY=1` 环境变量作为兼容开关，两者任一生效即落盘。

#### Scenario: 配置开启触发落盘
- **WHEN** config.json 设置 `"dump_kiro_body": true`
- **AND** 客户端发起一条 /v1/messages 请求
- **THEN** 最终发往 Kiro 的请求 body 写入 `app/config/kiro-body-dump/` 目录

#### Scenario: 默认关闭
- **WHEN** config.json 未设置 `dump_kiro_body`（或为 false）且未设置环境变量
- **THEN** 不落盘，行为与现状一致

#### Scenario: 环境变量兼容
- **WHEN** config.json 未开启，但环境变量 `KIRO_DUMP_BODY=1`
- **THEN** 仍触发落盘（向后兼容）

### Requirement: 日志可选写入文件
系统 SHALL 支持 `config.json` 中的 `log_file` 字符串配置项（可选，默认不设置）；配置后，tracing 日志在输出到控制台与 LogCapture 之外同步追加写入指定文件。

#### Scenario: 配置后写文件
- **WHEN** config.json 设置 `"log_file": "app/config/proxy.log"`
- **THEN** 服务启动后日志行追加写入该文件，且控制台与 LogCapture 输出不受影响

#### Scenario: 未配置零影响
- **WHEN** config.json 未设置 `log_file`
- **THEN** 行为与现状完全一致（仅控制台 + LogCapture）

#### Scenario: 文件不可写降级
- **WHEN** `log_file` 指向不可创建/不可写的路径
- **THEN** 服务启动不失败，仅记录一条警告日志并跳过文件写入

### Requirement: Dump 文件列表端点
系统 SHALL 提供 `GET /api/admin/dump-kiro-body/files`，返回 dump 目录内全部符合本功能命名规则的文件元数据，供 admin-ui 展示下载入口；目录不存在或为空时返回空列表而非错误。

#### Scenario: 目录内存在 dump 文件
- **WHEN** 客户端携带有效 admin key 请求 `GET /api/admin/dump-kiro-body/files`
- **AND** `app/config/kiro-body-dump/` 内存在 `body-1700000000000.json`（1024 字节）与 `body-1700000001000.json`（2048 字节）
- **THEN** 返回 200
- **AND** 响应体 `files` 数组含 2 项，每项包含 `name`（文件名）、`sizeBytes`（文件字节数）、`createdAtMs`（**取自文件名内的时间戳**，非文件系统时间，以保证与滚动清理的排序口径一致）
- **AND** 文件按名称排序，`totalBytes` 为 3072，`maxFiles` 为 200
- **AND** 响应含 `maxArchiveBytes`（归档总量上限），使调用方无需硬编码该阈值

#### Scenario: 目录不存在或为空
- **WHEN** 客户端携带有效 admin key 请求该端点
- **AND** dump 目录不存在，或存在但无任何符合命名规则的文件
- **THEN** 返回 200，`files` 为空数组，`totalBytes` 为 0（不返回错误，避免 UI 需区分"未开启开关"与"请求失败"）

#### Scenario: 目录内存在不符合命名规则的文件
- **WHEN** dump 目录内存在一个不符合 `body-<数字>[-<数字>].json` 命名规则的文件（如手工放入的 `notes.txt`、`backup.json`）
- **THEN** 该文件不出现在 `files` 列表中（仅暴露本功能自身产生的文件）

#### Scenario: 文件名带碰撞后缀的时间戳解析
- **WHEN** dump 目录内存在 `body-1700000000000-3.json`（为同毫秒内第 4 次落盘的冲突重试产物）
- **THEN** 该文件出现在列表中，其 `createdAtMs` 为 `1700000000000`（后缀不影响时间戳解析）

### Requirement: Dump 滚动清理与列表共用同一过滤谓词
系统 SHALL 使用同一命名校验规则（`body-<数字>[-<数字>].json`）既过滤列表端点返回值，也过滤滚动清理的计数对象，避免出现「目录内存在文件但列表为空」的不一致。

#### Scenario: 非本功能命名的文件不占用保留名额
- **WHEN** dump 目录内已有 200 个 `body-*` 文件，另存在一个手工放入的 `notes.json`
- **AND** 客户端发起一条新请求触发落盘
- **THEN** 清理按本功能命名规则计数，`notes.json` 不占用 200 名额
- **AND** 被删除的是最早的 `body-*` 文件，`notes.json` 保留在磁盘上
- **AND** `notes.json` 始终不出现在文件列表中（列表与清理对它的处理一致）

### Requirement: Dump 单文件下载端点
系统 SHALL 提供 `GET /api/admin/dump-kiro-body/files/{name}`，以附件形式下载指定的 dump 文件。

#### Scenario: 下载存在的文件
- **WHEN** 客户端携带有效 admin key 请求 `GET /api/admin/dump-kiro-body/files/body-1700000000000.json`
- **AND** 该文件存在于 dump 目录内
- **THEN** 返回 200，响应体为该文件原始字节（不修改、不重新序列化）
- **AND** `Content-Type` 为 `application/json`
- **AND** `Content-Disposition` 为 `attachment` 并携带该文件名
- **AND** 响应携带 `X-Content-Type-Options: nosniff`

#### Scenario: 请求不存在的文件
- **WHEN** 客户端请求的文件名符合命名规则但目录内不存在
- **THEN** 返回 404

#### Scenario: 文件名不符合命名规则
- **WHEN** 客户端请求 `{name}` 不匹配 `body-<数字>[-<数字>].json`
- **THEN** 返回 400，且不读取任何文件

### Requirement: Dump 目录路径穿越防护
系统 SHALL 阻止通过 `{name}` 路径参数读取 dump 目录之外的任何文件，且不向客户端泄露服务器文件系统的绝对路径。

#### Scenario: 含路径分隔符的请求
- **WHEN** 客户端请求的 `{name}` 含 `/`、`\` 或 `%2F`、`%5C` 等编码形式
- **THEN** 返回 400，不读取 dump 目录之外的任何文件

#### Scenario: 含相对路径跳转的请求
- **WHEN** 客户端请求的 `{name}` 为 `../config.json`、`..%2Fconfig.json`、`....//config.json` 等变体
- **THEN** 返回 400，不读取 dump 目录之外的任何文件

#### Scenario: 错误响应不含绝对路径
- **WHEN** 上述任一非法请求被拒绝
- **THEN** 响应体不包含服务端的绝对文件系统路径或目录结构信息

### Requirement: Dump 全量打包下载端点
系统 SHALL 提供 `GET /api/admin/dump-kiro-body/archive`，将 dump 目录内全部符合条件的文件打包为一个 ZIP 归档下载，且归档生成过程不得将整个目录一次性读入内存。

#### Scenario: 打包多个文件
- **WHEN** 客户端携带有效 admin key 请求该端点
- **AND** dump 目录内存在 3 个符合命名规则的文件
- **THEN** 返回 200，`Content-Type` 为 `application/zip`
- **AND** `Content-Disposition` 为 `attachment` 并携带带时间戳的文件名
- **AND** 归档可被标准解压工具（`unzip -t` / 系统归档程序）正确解压
- **AND** 解压后得到与源文件**逐字节一致**的 3 个文件，文件名与源文件名相同

#### Scenario: 空目录请求打包
- **WHEN** dump 目录不存在，或存在但无任何符合命名规则的文件
- **THEN** 返回 404（与列表端点的空数组语义不同：用户主动发起打包却无内容可打包，属于异常状态）
- **AND** 状态码在开始输出响应体之前确定（不出现"先 200 后中断"）

#### Scenario: 归档总量超过上限
- **WHEN** dump 目录内文件累计字节数超过归档总量上限
- **THEN** 归档中仅包含未超出上限的文件，且累积方向为**从最新文件到最旧文件**（截断时保留最近的取证数据）
- **AND** 每个文件按**整文件**粒度纳入或跳过，不产生半个文件
- **AND** 归档内附一条文字说明文件（`README.txt`）记录截断事实，该说明文件**计入归档条目总数**，且物理位置在所有 dump 数据之后、中央目录之前
- **AND** 返回仍为 200（部分内容优于失败，用户可再按需单独下载剩余文件）

#### Scenario: 未触发截断时不生成说明文件
- **WHEN** 全部文件均在上限内且无读取失败
- **THEN** 归档内不含 `README.txt` 条目

#### Scenario: 流式输出期间文件被滚动清理
- **WHEN** 归档已开始输出响应体（已返回 200），期间某待打包文件被滚动清理删除或读取失败
- **THEN** 该文件被跳过，归档仍可被正常解压（不产生损坏条目）
- **AND** 归档内的 `README.txt` 记录该文件名，**并计入归档条目总数**
- **AND** 不中断响应流、不改动已发出的状态码

#### Scenario: 内存持有量不随文件数增长（数据部分）
- **WHEN** 生成器产出归档字节流
- **THEN** 其持有的文件数据缓冲至多为**单个文件**的字节，不累积此前已发送文件的数据字节
- **AND** 仅中央目录元数据（每条约 30 字节）随文件数线性增长

### Requirement: Admin UI 提供 Dump 文件下载入口
admin-ui 的设置面板 SHALL 在「Kiro 请求体落盘取证」开关所在行内提供下载入口，使管理员无需登录服务器即可取回 dump 文件。

#### Scenario: 开关所在行存在下载入口

- **WHEN** 管理员打开设置页面
- **THEN** 「Kiro 请求体落盘取证」所在配置行的控件区同时呈现该开关与「下载」按钮
- **AND** 点击「下载」打开文件列表弹窗，展示文件名、大小与本地时间，最新文件在前

#### Scenario: 开关关闭时仍可下载历史文件

- **WHEN** `dumpKiroBody` 开关当前为关闭状态，但磁盘上仍存在此前落盘的 dump 文件
- **THEN** 下载入口保持可用，列表正常展示既有文件（开关只控制新请求是否落盘，不影响历史文件的取回）

#### Scenario: 空目录下的入口表现

- **WHEN** dump 目录不存在，或目录内无符合命名规则的文件
- **THEN** 弹窗显示空态提示（不显示错误，也不显示下载按钮）

#### Scenario: 下载请求携带 admin 认证

- **WHEN** 管理员从弹窗点击单个文件下载或「打包下载全部」
- **THEN** 请求通过 `x-api-key` 头携带 admin 凭据（**不使用查询参数**，避免凭据进入反向代理访问日志）
- **AND** 文件以浏览器下载方式落盘，文件名与源文件名一致（打包时为带时间戳的 zip 名）

#### Scenario: 文件在列表与下载之间被清理

- **WHEN** 管理员点击下载某文件，但该文件已被滚动清理删除
- **THEN** 界面提示文件已被清理、建议刷新列表，且不自动重试

#### Scenario: 归档超过体积上限时的提示

- **WHEN** 列表中全部文件累计体积超过后端下发的 `maxArchiveBytes`
- **THEN** 打包入口给出截断提示（阈值取自后端响应，不在前端硬编码）

#### Scenario: 窄屏与桌面端布局均正常

- **WHEN** 设置面板在小于 1024px 与大于等于 1024px 两档视口下渲染
- **THEN** 该配置行在窄屏下不溢出、不挤压既有开关
- **AND** 大于等于 1024px 的网页端功能与 UI 与变更前一致（项目硬约束）

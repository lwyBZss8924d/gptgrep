# GPTgrep

[English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

面向智能体的本地文档检索：**完整文档的 Rust 解析、原生 Codex 解析／OCR 智能体、文档树、内嵌 tgrep 1.1.0、
多模态 Decisions 路由／重排，以及本地 Codex 推理宿主**。
无需向量数据库、搜索守护进程或 MCP 服务器。

GPTgrep 返回可核查的源文档证据。默认 `search` 使用 `hybrid`，必须执行 Jev 路由与重排。
`ask` 和 `summarize` 也会在最终 Codex reader 之前执行这一阶段；缺少凭据或提供方调用失败会明确报错。
`semantic` 和 `judge` 同样使用远程 Jev 推理。显式的 `regex` 与 `lexical` 命令是本地检索原语。
推理智能体可以组合搜索、查看文档树，并在回答前分次读取有大小限制的节点内容。

协作者请先阅读[开发与评估环境](docs/development.md)和带日期的[实现与 live 运行状态](docs/experimental-status.md)。
统一运行时提供[四个 Skill 与三个原生角色](runtime/codex/STAGING.md)、
[覆盖全部原生操作的 incur CLI](packages/cli/README.md)，以及
[可直接调用的 Rust API 与可运行的组合示例](docs/rust-sdk.md)。
Document／OCR 调用方自行选择原生能力、权限、作业预算和包根目录；GPTgrep 会把选定的指引放入每个实际工作目录。
当前 `0.2.0-alpha.1` 开发线在固定的 XL-DocBench 任务集合上比较 **同一原生 Codex 智能体使用
GPTgrep 与裸 Codex**。两组接收相同问题、完整的原始文档上下文与输出要求，保留共同的原生工具；
GPTgrep 组额外接收固定版本的二进制、通用 Skills／角色和该上下文的完整预处理产物。
监督模型为 `gpt-6.1-sol`，worker 与 OCR 使用 `gpt-6-luna`，辅助模型显式选择 Luna Decisions 或 Jev 1.14。
比较前固定模型、运行时、权限与预算，分别记录配置与实际观测；这些比较配置不改变产品的 CLI 默认值。
查询前的准备成本单列，所有依赖问题的工作计入 SEARCH，判分成本另计。

[评估协议](docs/evaluation.md)规定当前验收条件，[基准工具包](benchmark/UPSTREAM.md)维护来源与任务契约，
[自优化工具包](self-opt/src/evaluate.ts)比较开发迭代。默认开发目标是在相同完整入组任务集合上取得更高的
有证据支持的成功率。发布／canary 结论还需要事先固定的完整确认集合、匹配比较优势和独立审查。
小规模 live／调试检查、离线测试、源码交付或仅含 GPTgrep 组的运行均不能代表完整 XL 结果或 canary 验收。
旧 PageIndex／full62 评估及其首发门槛已退役，历史测量不构成新的 XL 结果。版本与交付规则见 [SPEC](SPEC.md)。
请参阅[架构说明](docs/architecture.zh-CN.md)和明确列出的
[Flash 阶段覆盖情况](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md)。
维护者的研究记录保存在 `docs/research/` 下，仅供本地使用，不纳入版本控制。

`gptgrep enrich` 是本地 `index` 之后显式调用的模型辅助步骤。`--plan-only` 会完整遍历
规范化文档并固定有上限的工作计划，不调用模型。完成的 live 构建可由 Codex（builder 默认
`gpt-6-luna`）和 Jev 支持判别发布独立、来源绑定的导航信息层；生成的提示不能作为引用。
未完成工作保留账本，现有索引不变。限定文档的规划式 `ask` 可使用完成的 `enrich` 回执中的
`artifact_sha256`，通过 `--navigation-overlay-sha256` 显式启用：先由 Jev 完整扫描来源绑定的
提示，再交给规划器与 reader。普通 `ask` 保持原行为；提示质量及基准收益仍需 live 验证。

## 构建与使用

请先按[开发说明](docs/development.md)设置 `CARGO_TARGET_DIR` 及其他自有输出目录。

```sh
cargo build --release --locked
"$CARGO_TARGET_DIR/release/gptgrep" doctor --json
"$CARGO_TARGET_DIR/release/gptgrep" index ./documents --json
"$CARGO_TARGET_DIR/release/gptgrep" search 'retention|expiry' ./documents --mode regex --json
"$CARGO_TARGET_DIR/release/gptgrep" search 'signed snapshot recovery' ./documents --mode lexical --json
```

首次构建时，如果尚未提供所需文件，原生解析器依赖会下载固定版本的 PDFium 二进制文件。
原生发行压缩包包含 PDFium 运行库及其许可证声明；请使用其中的 `gptgrep` 启动脚本来选择随包提供的库。
Office 格式转换还需要 LibreOffice。默认 legacy index 关闭 OCR；显式 rich ingestion 和
文档 parse-agent 支持调用方选定的原生 OCR 运行时。必需 OCR 缺失时直接失败，不发布不完整的索引版本。
纯文本和 Markdown 操作无需加载 PDFium。
[发行打包说明](docs/release.md)记录了已验证的平台和迁移到新目录后的运行检查。

索引存放在 `documents/.gptgrep/` 下。隐藏或被忽略的下级条目、凭据文件、符号链接，
以及常见的构建和运行时目录均被排除。每次完整重建都会发布一个新的不可变索引版本。
构建失败会保留上一个版本。旧版本会一直保留，直到显式进行管理。

```sh
# Exact grep: no model or API key.
gptgrep search 'SNAP-[0-9]+' ./documents --mode regex -C 2 --json
gptgrep search --mode regex --fixed-strings --ignore-case --json -- '--flag-like text' ./documents

# Optional PageIndex scan-cost merge stage for native paginated documents.
gptgrep index ./documents --optimize-merge --json

# Discover the tree, then read an exact document_id:node_id.
gptgrep files ./documents --json
gptgrep tree manual.pdf --root ./documents --json
gptgrep read DOCUMENT_ID:NODE_ID --root ./documents --max-bytes 8192 --json
gptgrep status ./documents --json
gptgrep --schema
gptgrep --llms
```

`--limit` 限制返回的结果数。`--context` 默认为零。退出状态码为：成功或有匹配时为 0，
没有匹配时为 1，发生错误或排除了过期证据时为 2。JSON 以单个对象输出到 stdout；
诊断信息输出到 stderr。
节点读取会返回 `next_offset`；将该值传给 `read --offset` 即可继续读取。
偏移量按节点内的 UTF-8 字节计数，每个读取窗口都保留精确的源文件坐标。
这样，智能体便能分次检查较大的章节，而无需返回大小不受限制的响应。

对于由 `index --rich`/`--ocr`、`index --parse-agent-config` 或 rich core API 发布的 v2 索引版本，
`gptgrep batch --rich --root ./documents --input rich-operations.json` 接受
`gptgrep.batch.rich.request.v1`。其七个本地操作为 `inspect_documents`、`inspect_pages`、
`inspect_native_metadata`、`read_artifact`、`search_artifacts`、`inspect_visual` 和 `read_visual`；
`search_multimodal` 与 `rerank` 是显式的付费操作。一个 batch 只解析一次 `CURRENT`，
或使用显式指定的索引版本与清单 SHA。输入和输出均不超过 1 MiB，最多 64 个操作，
每一项在执行前都会预留输出空间。检查类操作返回已索引的元数据；文本读取和搜索保留强类型的
原生／OCR 证据及源文件检查。搜索游标覆盖有上限的 `max_hits` 结果窗口，调用方必须检查截断和是否已穷尽。
响应始终为 JSON，单项或全局失败时退出码为 2。读取器的字节计数描述的是部分 API 读取，
而非完整的物理 I/O。显式的 `search_multimodal` 还支持在同一个保留的读取器和有限 Decisions
上下文上执行必需的文档路由与文本／视觉混合重排。所有完整的输出／调用份额都在提交前完成准入；
不提供付费游标或降级方案。`budget.max_decisions_submissions` 默认为 8（硬上限 200），
且不会重置父级额度。选项、有序失败、有界遥测和观测限制见 [rich batch 组合](docs/rich-batch.md)。

`gptgrep rerank --root ROOT --input candidates.json --json` 只对调用方提供的、来自一个固定 rich
索引版本的原生／OCR 文本或视觉证据引用打分。它从已存储的产物解析内容，拒绝调用方提供的文本、
图像字节和 URL。等价的 rich batch `rerank` 操作与本地操作和 `search_multimodal` 共享固定的输出和调用预留。
显式 rerank 不执行文档路由；普通搜索仍保留必需的路由。两种 rerank 接口都不授予 reader 交付或引用权限。
严格的输入与结果合同见[来源绑定的 rerank](docs/rerank.md)。
默认 `index` 发布原始格式。显式的 `--rich` / `--ocr` 选择 rich ingestion；`--parse-agent-config`
配合完整的文档控制注册表选择原生智能体解析与自有产物导入。它们是具有明确运行时要求的不同接口，
见[开发说明](docs/development.md)。parse-agent 的图像文件、base64 和每页字节容量在控制文件的
`resources` 对象中配置。耦合的限制、原生 profile 绑定和保留的固定图像合同见
[4 MiB 图像示例](docs/architecture.zh-CN.md#原生解析与-ocr)。

Rust 核心为从 `VisualGenerationReader` rendition 得到的 PNG 裁剪提供 `DerivedVisualCache`。
每个派生图都绑定其索引版本、源文件、父图像、裁剪配方、坐标变换、编码器和输出摘要值。
缓存命中和读取都会重新检查父图像。payload、元数据和工作容量分别准入；保留的图像 pin 和配方 token
在逐出或 `clear` 之后仍保留其容量占用。这个进程内缓存保持已封存的索引版本不变，
不授予 reader 图像交付或引用权限。其上限覆盖自有缓存 payload 和裁剪准入，而非进程总内存。

## Jev 核心检索

必需的 Decisions 阶段默认使用 `openai/gpt-6-luna-decisions`。显式 `--model typesafe/jev-1.13`
选择旧版纯文本 Jev。默认搜索执行文档路由和证据重排；不会自动降级到纯本地检索。
默认解析路径在本地执行；显式 OCR、document-agent 与 enrich 会在 INDEX 阶段调用模型，
发布仍要求完整且已验证的产物。
使用 `--document manual.pdf` 可在候选预算生效之前，将检索限制到一个精确的已索引路径。
结果会报告 `document_scope` 和 `coverage`。

请通过现有的密钥管理工具，在进程环境中提供 `OPENROUTER_API_KEY`。
GPTgrep 不会自动加载凭据文件。可选的开发辅助脚本 `scripts/with_dev_key.py`
只读取显式选定的条目，不执行环境文件中的 shell 代码、不复制环境文件，也不回显密钥值。

```sh
gptgrep search 'how can a damaged journal be recovered?' ./documents \
  --model typesafe/jev-1.13 --min-score 0.5 --json

gptgrep judge --input ./decision-request.json \
  --model '~typesafe/jev-latest' --json
```

响应保留实际返回的模型元数据。准入要求返回请求的标识，或其已观察到的规范版本
（Luna 为 `openai/gpt-6-luna-decisions-20261006`，旧版 Jev 为 `typesafe/jev-1.13-20260917`）；
其他显式模型要求标识完全一致。不接受猜测的带日期标识、其他系列或回退模型。
原生 Codex 推理独立使用 `gpt-6-luna`、`max` 和 `fast`。

`~typesafe/jev-latest` 是 OpenRouter 别名：目录将其解析为 Jev 系列的最新版本，响应返回实际作答的版本。
只有当返回的模型是同一作者、同一系列的具体版本（`author/family-VERSION`，且版本以数字开头）时，
GPTgrep 才接受 `~author/family-latest` 别名。对该别名而言，`typesafe/jev-1.13` 和
`typesafe/jev-1.13-20260917` 会被接受；其他作者、其他系列（例如 `typesafe/jevx-2`）、`:free`
变体或另一个别名都会被拒绝。这种拒绝发生在调用已计费之后。格式错误的别名会在发出任何请求之前被拒绝。
别名仅支持纯文本：typed 图像和原始图像部分会在发出请求前被拒绝。报告分别保留请求的选择器和实际返回的模型；
judge 输出在返回的 `model` 之外报告 `requested_model`。需要结果保持可比时，请固定规范版本。

Jev 使用 OpenRouter 的强类型 **Decisions API**，而非 chat completions。
它提供动态的 Choice/Noul/Score 判断；解析、偏移量、算术计算和工作流执行仍由应用代码负责。
响应会记录实际返回的模型，以及可获得的用量和费用。未知指标保持为 null。
网络或响应结构错误会明确报出；不会静默回退或跟随重定向。默认检索的路由／重排在收到
HTTP 429/500/502/503/504 时最多重试一次，且不超出原始请求时限与提交额度；失败的尝试仍会报告，
其费用保持未知。重试前等待 250–1000 ms 的抖动退避，或更长的整数秒 `Retry-After`。
其他形式的 `Retry-After`（HTTP 日期、重复字段，或负数、小数、超过十位的数值）会拒绝重试，
并记录为 `retry_after_refusal`。只有当原始时限仍能容纳该等待再加 2 秒、且仍有剩余提交额度时，
才会发送重试。超时和其他失败不重试。

混合搜索将词法候选与经语义路由选出的树节点结合起来。初始策略最多路由 32 个文档描述，
并对最多 24 个证据候选进行重排。只有一个可路由文档时（例如 `--document` 范围），
该文档会被直接选中，不发起路由调用（见 `coverage.routing_status`）。
在将结果视为穷尽性结果之前，请先检查 `coverage`。
`--min-score 0.5` 是相关性评分规则的最低门槛，**不是经过校准的置信度**；
`--min-score 0` 会展示低分候选，便于探索。大规模文档集合的分层语义路由仍是明确待完成的扩展性任务。
对于单个完整词元的精确查询，即使 Jev 给出低分，混合搜索的词法路径也会保留经过验证的词元匹配。
结果会显示 `literal_anchor`，以及低于门槛但仍被保留的结果数量；模型分数保持原值。
若没有候选达到门槛，仍会返回分数最高的三个低置信度候选，标记 `below_threshold: true`，
并附带 `coverage.below_threshold_fallback` 与警告；它们从不算作通过门槛。由于返回了结果，
退出码为 0；纯文本输出将这些候选打印为 grep 上下文行（`PATH-LINE-TEXT`），而不是
`PATH:LINE:TEXT` 匹配，因此请检查 `--json` 输出中的 `below_threshold`。

## Decisions judge 与 typed 图像

`gptgrep judge --input evidence.json --json` 也接受常规的 typed 图像合同：`input.text` 包含
`{source, text}` 对象，`input.images` 包含指向本地 PNG/JPEG 文件的 `{source, path}` 对象，与
`questions` 并列。每个 `source` 包含 `source_id`、`unit_id` 和 `source_sha256`。源摘要值是调用方
提供的来源信息，而非独立的新鲜度声明。文本按字面处理；图像字节以有界快照的形式准入，并按顺序作为扁平的
`image_url` data URL 发送。源绑定和本地路径不会进入提供方请求体。64 KiB 控制／文本封装、最多四张图像、
每张 1 MiB／800 万像素、合计 2400 万像素、8 MiB 请求和 1 MiB 响应的上限均明确适用。
旧版 Jev、其他精确模型和所有别名都会在预留或提交前拒绝 typed 图像。原始 `{state, questions}` 输入
适用同一规则：对这些选择器，`state` 或 `questions` 中任何带有 `image_url` 或 `input_image` 成员或
`type` 的对象都会在预留前被拒绝。字符串保持字面文本。Luna 原样接收原始 JSON。
拒绝、未知答案、不完整的响应体和格式错误的响应都不会成为成功的判断。

judge 成功时打印已验证的响应，其中 `model` 是实际返回的版本，另附 `requested_model`。
judge 失败时以退出码 2 输出 `gptgrep.error.v1`，代码为 `decisions_judge_failed`，并带有 `judge`
对象（`gptgrep.judge-failure.v1`）。该对象包含阶段、请求的与实际返回的模型，以及提供方响应 ID。
它还记录发送是否已开始（`not_submitted`、`sent_response_received` 或 `sent_outcome_unknown`）。
可获得时报告用量和 `cost_usd`；否则为 null，并附原因，例如 `not_submitted`、
`http_status_body_not_read` 或 `cost_null`。该对象还包含延迟、失败诊断和上下文的准入快照。
它从不包含请求体或响应体、header、URL 或密钥。

## 共享 Decisions 预算

进行共享实验时，可为 hybrid/semantic 搜索（包括 `--multimodal`）、judge、ask、summarize、
非 plan-only 的 enrich、显式 `rerank` 和付费的 `batch --rich` 操作加上 `--decisions-budget budget.json`。
可移植的控制文件最大 16 KiB，包含 `schema_version: "gptgrep.decisions-budget-contract.v1"`、
`run_id`、`plan_digest`、`deadline_unix_ms`、`max_submissions` 和绝对路径 `ledger_path`。
该选项加入一个已存在且匹配的账本：沿用其原始时钟和已消耗的预留。文件缺失，或 run、截止时间、
plan 或上限发生变化，都会报错；它从不创建或重置账本。新账本只需创建一次：

```sh
gptgrep decisions-budget init --run-id smoke-1 --plan-file plan.json \
  --deadline-seconds 1800 --max-submissions 2 \
  --ledger /abs/private/ledger.json --output /abs/private/budget.json
gptgrep decisions-budget status --control /abs/private/budget.json --json
```

`init` 将 `plan_digest` 设为 plan 文件（不超过 1 MiB 的普通文件）的 SHA-256，并将截止时间设为
当前时间加 N 秒（1–86,400，从不续期）；`--max-submissions` 接受 1–200。时钟锚点绑定创建它的
启动周期与时钟命名空间，因此重启后加入会失败关闭（fail closed）。它会连同原始时钟锚点创建 journal
及其 `.lock` 附属文件，然后以 0600 权限一次性写入控制文件。它拒绝相对路径以及任何已存在的账本、
锁或控制文件路径；从不加入、重置、续期或替换。`status` 只读，按结果状态（`reserved`、`in_flight`、
`unknown`、`terminal`）报告预留、剩余提交次数、请求的选择器、截止状态以及准入是否开放；截止后仍可读取。
预留是永久的：已加入的运行在达到 `max_submissions` 时，会在任何 POST 之前以
`Decisions submission budget exhausted` 停止。路由、重排和宿主后续客户端共享同一上下文。
不使用该选项时，调用使用有界的单次调用上下文。regex/lexical、纯本地 batch、schema、doctor
和 enrich `--plan-only` 不会打开预算文件；`parse-agent`、`ocr serve`、`index --parse-agent-config`
和 `decisions-budget` 子命令会拒绝该全局选项。数值用量和费用仍是提供方报告的元数据；
缺失字段保持不可用。账本统计的是提交次数，而非美元。任何支出上限都在产品外部执行，
例如按提交次数乘以最坏情况下的单次调用价格。

## 本地 Codex 宿主

[config/codex-host.toml](config/codex-host.toml) 中专用辅助模型的默认配置如下：

```toml
model = "gpt-6-luna"
model_reasoning_effort = "max"
service_tier = "fast"
approval_policy = "never"
sandbox_mode = "read-only"
```

需要已经安装 Codex，并有已完成认证的本地账户。高级 Document／OCR 路径选择一个固定版本的独立
app-server 和来源绑定的控制文件。保留的固定图像 profile 使用
[app-server 派生版本](patches/codex-native-viewer/README.md)及其独立的资格验证。普通 CLI
负责设备登录和所选的 reader 工作流。[开发说明](docs/development.md)解释了这些配置选择。
进行隔离开发时，请显式选择账户主目录。认证由 Codex 处理；GPTgrep 不读取或复制其认证缓存，
也不安装全局配置。如果尚未登录，可参阅 [Codex 认证文档](https://learn.chatgpt.com/docs/auth?surface=cli)
中的无图形界面设备码登录流程。

```sh
gptgrep ask 'What is the recovery procedure, and how long are snapshots kept?' \
  ./documents --json

gptgrep summarize DOCUMENT_ID:NODE_ID --root ./documents \
  --model gpt-6-luna --reasoning-effort max --service-tier fast --json
```

默认模式在启动 reader 之前，宿主会执行必需的 Jev 混合检索，并将有大小限制的证据交给推理模型。
`--jev-model` 选择 Decisions 模型，与 Codex 的 `--model` 分开配置。
`--document` 可限制问答范围；摘要则限制到所选节点所属的文档。
后续搜索默认使用 hybrid；树节点读取和显式精确检索可在初始结果基础上继续补充证据。
报告同时保留 Jev 的覆盖范围、实际模型和用量，以及 Codex 用量。

文本检索 reader（受限模式或原生模式）还会收到通用回答规则。这些规则要求它按问题要求的形式先给出直接答案，
并通过阅读被引用的段落来解析问题所依赖的文内引用
（注释标记、引文编号、“见表”或“见节”之类的指引）。这些规则从不放宽证据、引用或
`insufficient_evidence` 规则。富文本与多模态 reader 不接收这些规则。它们属于 GPTgrep 的 reader 合同。

在默认的受限模式下，宿主会通过 stdio 创建临时 app-server 线程，其执行环境为空，并提供受限的 GPTgrep
目录、树、搜索和读取工具。它会验证实际生效的沙箱与审批设置，限制运行时间和工具调用次数，
拒绝意外的服务器请求，并终止、回收自己创建的子进程。模型生成的引用必须对应本次运行中实际返回过的证据，
且仍与当前源文件一致。报告分别记录答案、证据、工具回执、实际模型与推理强度，以及用量。
宿主还会在 `.gptgrep/host-attempts/` 下写入私有且有大小限制的元数据记录，
使后续阶段失败或被中断时，已完成的 Jev 调用仍可核查。源文档保持不变；
Codex 子进程无法访问 Jev 凭据。
引用身份检查本身不能独立证明证据在语义上支持答案。

`ask --native-ptc --codex-permissions PROFILE` 则在一个已存在的命名 Codex 权限 profile 下，
以原生执行方式运行最终检索 reader。运行时必须在 turn 开始前确认该 profile，并省略旧版沙箱覆盖设置。
reader 继承所选 Codex 的能力和 Skill，并启用 shell 和图像查看。观察到的原生条目与 GPTgrep 调用
共享 `--max-tool-calls`（默认 12）。原生条目通知可能在执行开始后才到达，因此该预算是计量，
而非执行前的拦截；超出预算的请求或观察到的条目会使 `ask` 以
`host_native_ptc_tool_budget_exhausted` 失败，且不给出答案。受限 reader 对 `--timeout` 的 900 秒
上限和对 `--max-tool-calls` 的 64 次上限在此模式下不适用。原生输出不会产生引用：答案仍须引用
GPTgrep 实际交付的证据。查询规划器和 `host-complete` 仍不使用工具。

仅 `ask` 支持的 `--experimental-query-plan` 默认会先运行一个独立、无工具的
`gpt-6-luna` / 与调用方一致的推理强度 / `fast` 规划器（默认推理强度仍为 `max`）。
可用 `--planner-model` 显式指定其他模型，
例如在对照实验中使用 `gpt-5.6-luna`。它保留原问题，最多提出两条替代检索表述。
最多两路文档路由同时执行；候选按真实来源跨度合并，仍受原候选预算限制，随后由 Jev
按原问题统一重排，再启动 reader。该选项默认关闭，所有阶段共享调用方的总截止时间。
规划或分支失败会明确报错。新增候选可能挤掉原候选并增加延迟，不保证质量提升。
`model_attempts` 和 `model_usage` 单独记录规划调用；旧的 `usage` 仍只表示 reader。

```sh
gptgrep ask 'How are offline exports recovered?' ./documents \
  --experimental-query-plan --json

# enrich 完成后，传入其 artifact_sha256 和精确的文档路径。
gptgrep ask 'How are offline exports recovered?' ./documents \
  --document manual.pdf --experimental-query-plan \
  --navigation-overlay-sha256 "$OVERLAY_SHA" --json
```

在上述命令中增加 `--experimental-evidence-roles`，可以测试默认关闭的
`jev-evidence-role-v1` 选择策略。Jev 保留相关性 Score，并为每个候选增加一次 Choice：
直接支持、局部证据不完整、背景或无支持。原查询 literal anchor 的优先级、相关性下限和
最多三个初始结果保持不变。角色排序及绑定到确切窗口的提示帮助 reader 选择证据、
在来源附近续读；实际交付证据集的充分性仍未评估。一个角色可以支持问题中的某项事实，
但不代表覆盖整个问题。打包若改变被评估的字节，交付窗口的角色提示就不可用。
被舍弃候选的诊断不携带被拒绝的正文，也不会使其获得引用权限。

最多 24 个候选在同一次统一 Decisions 请求中产生 48 个结构化问题，完整编码请求体必须
满足 64 KiB 上限。额外判断可能增加用量和延迟；后续 reader 的工作仍受预算约束并计量。
该选项本身不增加初始 Jev 操作或 Codex 步骤，必须与 `--experimental-query-plan` 一起使用，
失败时不会启用降级方案。

`--experimental-source-continuation`（默认关闭；需要 `--experimental-query-plan` 和 `--document`）
只为 reader 增加指导。`next_offset = null` 只表示一个节点结束，而非调查结束，因此 reader 会在现有的
工具、时间和输出预算内，通过限定范围的树／读取节点核实尚未解决的事实，否则返回
`insufficient_evidence`。它不增加模型调用，不为已发放区间以外的内容授予引用权限，其质量作用尚未测量。

在默认的受限 reader 中，达到检索工具预算（`--max-tool-calls`，默认 12）后，后续请求会被拒绝，
并指示其在同一 Codex 轮次和原截止时间内完成回答。被拒绝的请求不会提供新证据，也不会执行 Jev；
其诊断计数与模型轮次计数分开记录。如果被拒绝的请求也达到该预算，下一次请求会使 `ask` 以
`host_tool_budget_exhausted`（退出码 2）失败，且不给出答案。`node_coverage` 只描述当前返回片段，
不能代表累计阅读范围：到达 EOF 时，该片段仍可能未包含前缀。

`host-complete --input FILE_OR_DASH` 将同一个隔离的本地模型提供为强类型工作流基础操作。
其输入为 `{instructions, state, schema}`，输出包含通过 schema 校验的 `value`、
模型与推理强度、用量及输入摘要值。默认总输入预算为 256 KiB，可以显式调整，最高为 1 MiB；
提示词绝不会被静默截断。此补全模式不声明具备引用验证能力。

app-server API 尚处于实验阶段。已支持的能力边界和配置记录在
[宿主 crate 文档](crates/gptgrep-host/README.md)中。

## 多模态检索

`search --multimodal` 和 `ask --multimodal` 将 v2 rich 索引版本接入常规的 Decisions 与原生 reader 工作流：

```sh
gptgrep search 'What does the chart show?' ./documents --multimodal --max-visual-candidates 8 --json
gptgrep ask 'Explain the chart and supporting text.' ./documents --multimodal --document report.pdf --max-citations 4 --json
```

两个命令都只解析一次 `CURRENT`，保留该来源绑定的索引版本和一个有限的 Decisions 执行上下文，
并依次执行必需的文档路由，以及对文本候选和独立视觉候选的重排。视觉通道可以包含没有字面查询匹配的页面。
默认 Decisions 模型为 `openai/gpt-6-luna-decisions`；多模态检索也接受其已观察到的规范标识
`openai/gpt-6-luna-decisions-20261006`。多模态 ask 需要独立的原生 `gpt-6-luna/max/fast` profile。
显式的旧版 Jev 仍可用于文本工作流。

多模态输出始终是一份 typed JSON 报告，即使省略 `--json` 也是如此。搜索返回
`gptgrep.rich_multimodal_search.v1`；其中的图像暴露不授予引用权限。ask 返回
`gptgrep.rich_multimodal.host.v1`，并保留初始检索报告。文本和图像引用都要求原生 reader 实际完成完整交付，
并通过来源、哈希和坐标检查。部分失败会在现有的 `gptgrep.error.v1` 封装中保留已观察到的检索／计量信息，
并以退出码 2 结束。

可配置的上限包括 `--window-bytes`（4–2048）、`--max-visual-candidates`（1–16）、
`--max-concurrent-batches`（1–4）、`--max-image-bytes`（最多 1 MiB）、`--max-image-pixels`（最多 800 万）、
`--max-metadata-bytes`（最多 512 KiB）和 `--max-output-bytes`（最多 1 MiB）。ask 还接受 1–8 个
`--max-citations`，并适用宿主的 32 KiB 候选元数据上限和 512 KiB 初始报告上限。预算过紧时，
可能在模型调用前就拒绝准备。报告会区分已检查／被省略的候选、截断和不完整的语义覆盖。
这些限制描述的是自有缓冲区和传输，并非整个进程 RSS 或穷尽召回的保证。

`--multimodal-timeout` 接受 1–900 秒，搜索默认 180 秒；ask 取该值与其现有 `--timeout` 中较小者。
`--decisions-budget FILE` 为路由、重排和宿主 Decisions 工具加入同一个已存在的原始时钟共享上下文，
不初始化或重置其账本。不提供该文件时，命令会创建一个有限的单次调用上下文，其 plan 身份绑定所保留的
索引版本清单；这不代表可移植的实验活动额度。

v1 索引版本、regex/lexical/semantic 模式选择、旧版 grep／窗口／候选选项，以及 ask 的实验性
规划／导航／续读选项，在该工作流中都会被拒绝。不存在文本或 regex 降级。省略 `--multimodal` 时保持旧路径和输出不变。
rich ingestion 入口支持无损 UTF-8 文本／Markdown、PDF、PNG 和受限的严格 baseline JPEG。纯文本保留原始字节和行区间，
不声明物理页或 OCR。Office 输入使用一次有界的 LibreOffice 转换，转换结果供解析、渲染和 OCR 共用；
需要可用的转换器。现有限定范围的 Office 证据不能证明支持所有 Office 格式或文档。更广泛的 JPEG 变体仍不受支持。
CLI 组合与离线模拟检查只能证明实现行为。限定范围的原生图像资格验证，与完整文档集合的原生索引、
当前源码的提供方结果，以及仍待完成的成对比较／发布门槛彼此独立。

## 前台 OCR

前台 OCR 由原生 `gptgrep ocr serve --config CONTROL_JSON --token-env GPTGREP_OCR_SERVE_TOKEN` 入口提供；
可选 incur 包装层的 `ocr serve` 在前台监管同一个原生进程（仅限 POSIX），不会增加第二个服务。
它使用经过认证的 loopback、由服务端显式持有的来源／任务托管，以及有限的原始锚点阶段。
完整任务与 PNG LiteParse 兼容模式分别进行资格验证并记录来源；兼容模式的置信度由模型报告，未经校准。
控制 schema、仅创建账本的行为、限制和待完成的运行时资格验证见[前台 OCR](docs/ocr-serve.md)。
普通查询不会启动 OCR。

## 可选的 incur 接口

原生二进制程序提供 grep 接口。`packages/cli` 通过 `gptgrep-ai` 添加实际的 incur
schema 与接口发现能力，将强类型请求对象转发给原生可执行程序，不经过 shell 插值。
包装层禁用了 MCP 服务、Skill 同步和更新入口。

```sh
cd packages/cli
pnpm install --frozen-lockfile
GPTGREP_BIN=/absolute/path/to/gptgrep node src/cli.js --schema
GPTGREP_BIN=/absolute/path/to/gptgrep node src/cli.js search \
  --request '{"query":"retention","root":"/absolute/path/to/documents","mode":"lexical"}'
```

完整的请求字段见[包装层文档](packages/cli/README.md)。包装层固定使用已发布的 incur 0.5.1；
本次研究中查看的本地 0.6.0 检出版本，当时尚未作为版本发布到包注册表。

## 验证与证据

运行这些检查之前，请先按[开发说明](docs/development.md)取得经存储所有者核验的外置运行目录。
当前离线基准与自优化契约测试使用 Bun，不需要安装 npm 依赖。

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
bun run --cwd benchmark test
```

测试样例覆盖正则匹配正确性、词法检索、Unicode、精确的源文件摘要值与字节区间、
源文件被替换或删除后的过期状态，以及重新索引。原生解析器测试覆盖实际生成的 PDF、书签、
无文本层文档的拒绝处理，以及超过 1,000 页的文档。合并阶段的测试样例会执行固定版本上游的纯函数，
以进行对比。模拟 Codex/HTTP 测试用于验证传输层必须保持的性质；真实宿主或提供方调用的回执属于另一类证据。

[XL 来源契约](benchmark/UPSTREAM.md)固定发布版本、评分器、文档权利与本地布局。
每个任务必须验证完整原始上下文。索引准备只接收原始文档，不接收问题、标准答案或证据标注；
reader 接收任务问题，但不得接收标准答案、证据标注或判分状态。
以下入口使用调用方选定的外置路径；采集与 live 提供方调用需要相应授权，各 reader 与独立 judge
必须有各自已准入的存储、认证状态和权限。配置字段与隔离要求见[评估协议](docs/evaluation.md)。

```sh
# 仅从已验证的本地材料选择开发样本。
bun benchmark/src/cli.ts freeze \
  --release "$XL_RELEASE_ROOT" --corpus "$XL_CORPUS_ROOT" \
  --output "$XL_RUN_ROOT/cohort" --seed "$XL_SEED" \
  --sample-size "$DEV_SAMPLE_SIZE"

# 每次调用只监督一个独立子进程。
bun benchmark/src/cli.ts reader --config "$READER_CONFIG"
bun benchmark/src/cli.ts judge --config "$JUDGE_CONFIG"

# 比较已保存的测量；本命令不执行实验。
bun self-opt/src/cli.ts compare \
  --incumbent "$INCUMBENT_RUN" --candidate "$CANDIDATE_RUN" \
  --output "$XL_RUN_ROOT/opt-comparison.json"
```

`freeze` 只登记开发样本。原生 XL 准确率与独立的证据支持成功率／原子覆盖率分别报告；
已启动的 reader 操作失败在证据支持成功率中计为零，准备或判分不可用保持未知，并保留固定分母。
缺失的用量、请求数与费用保持不可用。`self-opt` 报告始终保留 `releaseAccepted: false`。
详细的本地研究和实验解读仍保存在被忽略的 `docs/research/` 路径下。

## 当前版本的限制

- 布局前端由 LiteParse 提供。Rust 的结构与合并阶段不声明与完整 Flash 的字符／字体修复、
  多语言标题分类和生成式扩展算法等价。
- 原生 OCR 已实现，图像组件已有限定范围的 live 证据。解析／OCR 的完整原生 PTC 编码
  （包括原生内置工具，以及通过可用 CLI／API 完成的 shell、图像和 CV 处理）仍在进行中；
  固定 Page/Document 守卫的资格验证只覆盖单个组件。完整文档集合的 OCR／索引和所有 Office／图像格式的
  验收仍未完成；缺失或无效的必需提取会明确失败。
- 默认（v1）索引在任何受支持的文档没有可提取文本（例如扫描版 PDF）时，会拒绝整个构建，
  上一个索引版本保持当前状态。这类文档集合需要使用 `--ocr auto` 的显式 rich ingestion 或 parse-agent 索引。
- 语义候选预算可能使相关文档或节点未被选中。Jev 无法补回上游已排除的证据；其分数也不保证内容真实。
- 文档集合的新鲜度以快照为准。新增文件需要重新索引；搜索仅重新检查与本次检索相关的源文件，
  而 `status` 会检查现有索引中的全部文档。
- 当前索引操作会重建整个索引版本；增量复用解析结果，以及更广泛的性能与质量基准评估仍是后续工作。

原创代码采用 [MIT 许可证](LICENSE)。依赖项和原生运行库的许可证声明见
[THIRD_PARTY.md](THIRD_PARTY.md)。

# GPTgrep

[English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

面向智能体的本地文档检索工具：**Rust 文档解析与文档树、内嵌 trigram grep、作为核心的 Jev 路由与重排，以及本地 Codex 推理宿主**。
无需向量数据库、搜索守护进程或 MCP 服务器。

GPTgrep 返回可核查的源文档证据。默认 `search` 使用 `hybrid`，必须执行 Jev 路由与重排。
`ask` 和 `summarize` 也会在最终 Codex 回答 worker 之前执行这一阶段；缺少凭据或提供方调用失败会明确报错。
`semantic` 和 `judge` 同样使用远程 Jev 推理。显式的 `regex` 与 `lexical` 命令是本地检索原语。
推理智能体可以组合搜索、查看文档树，并在回答前分次读取有大小限制的节点内容。

协作者请先阅读[开发与评估环境](docs/development.md)和[当前实验状态](docs/experimental-status.md)。
原生 parse-agent／OCR、rich 索引和 full62 Harbor adapter 已有实现；完整 34 文档流水线
和 62 对 SEARCH 消融尚未验收。主比较使用相同 Codex reader，INDEX 不计入 SEARCH reward，
与 PageIndex G5 首发门槛分别判定。构建前按 setup 设置 `CARGO_TARGET_DIR` 等外置输出目录。

这是开发中的源码。第一个实验版发布前，必须在 PageIndex-OSS-Benchmark 的真实运行中，
证明相对 PageIndex Flash 加 GPT-5.6 的最小优势。请参阅[架构说明](docs/architecture.zh-CN.md)和明确列出的
[Flash 阶段覆盖情况](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md)。
原始本地 PageIndex OSS `results.json` 在 Luna/high 下为 60/62；另行验收的 SDK/Codex live 对照
在 Luna/max 下同为 60/62。两次运行的模型、后端和证据来源分别记录。
维护者的研究记录保存在 `docs/research/` 下，仅供本地使用，不纳入版本控制。

`gptgrep enrich` 是本地 `index` 之后显式调用的模型辅助步骤。`--plan-only` 会完整遍历
规范化文档并固定有上限的工作计划，不调用模型。完成的 live 构建可由 Codex（builder 默认
`gpt-6-luna`）和 Jev 支持判别发布独立、来源绑定的导航信息层；生成的提示不能作为引用。
未完成工作保留账本，现有索引不变。限定文档的规划式 `ask` 可使用完成的 `enrich` 回执中的
`artifact_sha256`，通过 `--navigation-overlay-sha256` 显式启用：先由 Jev 完整扫描来源绑定的
提示，再交给规划与回答 worker。普通 `ask` 保持原行为；提示质量及基准收益仍需 live 验证。

## 构建与使用

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
文档 parse-agent 支持经过资格验证的原生 OCR。必需 OCR 缺失或无效时，拒绝发布不完整索引。
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

## Jev 核心检索

Jev 是 GPTgrep 搜索系统的必需组成部分。默认搜索执行文档路由和证据重排；
不会自动降级到纯本地检索。默认模型为 `openai/gpt-6-luna-decisions`。
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

gptgrep judge --input evals/requests/decision-smoke.json \
  --model '~typesafe/jev-latest' --json
```

Jev 使用 OpenRouter 的强类型 **Decisions API**，而非 chat completions。
它提供动态的 Choice/Noul/Score 判断；解析、偏移量、算术计算和工作流执行仍由应用代码负责。
响应会记录实际返回的模型，以及可获得的用量和费用。未知指标保持为 null。
网络或响应结构错误会明确报出；不会静默回退或跟随重定向。默认检索的路由／重排在收到
HTTP 429/500/502/503/504 时最多重试一次，且不超出原始请求时限与提交额度；失败的尝试仍会报告，
其费用保持未知。超时和其他失败不重试。

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

## 本地 Codex 宿主

[config/codex-host.toml](config/codex-host.toml) 中专用辅助模型的默认配置如下：

```toml
model = "gpt-6-luna"
model_reasoning_effort = "max"
service_tier = "fast"
approval_policy = "never"
sandbox_mode = "read-only"
```

需要已经安装 Codex，并有已完成认证的本地账户。进行隔离开发时，请显式选择账户主目录。
认证由 Codex 处理；GPTgrep 不读取或复制其认证缓存，也不安装全局配置。
如果尚未登录，可参阅 [Codex 认证文档](https://learn.chatgpt.com/docs/auth?surface=cli)
中的无图形界面设备码登录流程。

```sh
gptgrep ask 'What is the recovery procedure, and how long are snapshots kept?' \
  ./documents --json

gptgrep summarize DOCUMENT_ID:NODE_ID --root ./documents \
  --model gpt-6-luna --reasoning-effort max --service-tier fast --json
```

默认模式在启动回答 worker 之前，宿主会执行必需的 Jev 混合检索，并将有大小限制的证据交给推理模型。
`--jev-model` 选择 Decisions 模型，与 Codex 的 `--model` 分开配置。
`--document` 可限制问答范围；摘要则限制到所选节点所属的文档。
后续搜索默认使用 hybrid；树节点读取和显式精确检索可在初始结果基础上继续补充证据。
报告同时保留 Jev 的覆盖范围、实际模型和用量，以及 Codex 用量。

宿主会通过 stdio 创建临时 app-server 线程，其执行环境为空，并提供受限的 GPTgrep
目录、树、搜索和读取工具。它会验证实际生效的沙箱与审批设置，限制运行时间和工具调用次数，
拒绝意外的服务器请求，并终止、回收自己创建的子进程。模型生成的引用必须对应本次运行中实际返回过的证据，
且仍与当前源文件一致。报告分别记录答案、证据、工具回执、实际模型与推理强度，以及用量。
宿主还会在 `.gptgrep/host-attempts/` 下写入私有且有大小限制的元数据记录，
使后续阶段失败或被中断时，已完成的 Jev 调用仍可核查。源文档保持不变；
Codex 子进程无法访问 Jev 凭据。
引用身份检查本身不能独立证明证据在语义上支持答案。


仅 `ask` 支持的 `--experimental-query-plan` 默认会先运行一个独立、无工具的
`gpt-6-luna` / 与调用方一致的推理强度 / `fast` 规划 worker（默认推理强度仍为 `max`）。
可用 `--planner-model` 显式指定其他模型，
例如在对照实验中使用 `gpt-5.6-luna`。它保留原问题，最多提出两条替代检索表述。
最多两路文档路由同时执行；候选按真实来源跨度合并，仍受原候选预算限制，随后由 Jev
按原问题统一重排，再启动回答 worker。该选项默认关闭，所有阶段共享调用方的总截止时间。
规划或分支失败会明确报错。新增候选可能挤掉原候选并增加延迟，不保证质量提升。
`model_attempts` 和 `model_usage` 单独记录规划调用；旧的 `usage` 仍只表示回答 worker。

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
最多三个初始结果保持不变。角色排序及绑定到确切窗口的提示帮助回答 worker 选择证据、
在来源附近续读；实际交付证据集的充分性仍未评估。一个角色可以支持问题中的某项事实，
但不代表覆盖整个问题。打包若改变被评估的字节，交付窗口的角色提示就不可用。
被舍弃候选的诊断不携带被拒绝的正文，也不会使其获得引用权限。

最多 24 个候选在同一次统一 Decisions 请求中产生 48 个结构化问题，完整编码请求体必须
满足 64 KiB 上限。额外判断可能增加用量和延迟；后续回答 worker 的工作仍受预算约束并计量。
该选项本身不增加初始 Jev 操作或 Codex 步骤，必须与 `--experimental-query-plan` 一起使用，
失败时不会启用降级方案。

检索工具预算耗尽后，回答 worker 在原 Codex 轮次和原截止时间内完成回答。被拒绝的请求不会提供新证据，也不会执行 Jev；其诊断计数与模型轮次计数分开记录。`node_coverage` 只描述当前返回片段，不能代表累计阅读范围：到达 EOF 时，该片段仍可能未包含前缀。

`host-complete --input FILE_OR_DASH` 将同一个隔离的本地模型提供为强类型工作流基础操作。
其输入为 `{instructions, state, schema}`，输出包含通过 schema 校验的 `value`、
模型与推理强度、用量及输入摘要值。默认总输入预算为 256 KiB，可以显式调整，最高为 1 MiB；
提示词绝不会被静默截断。此补全模式不声明具备引用验证能力。
它支持已声明的提供方桥接接口，用于在对比评估中运行原版 PageIndex 的摘要与优化提示词。

app-server API 尚处于实验阶段。已支持的能力边界和配置记录在
[宿主 crate 文档](crates/gptgrep-host/README.md)中。

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

Python 验证需要 3.12 或更新版本；下面的命令会在本地隔离环境中安装固定版本的依赖。

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
uv venv --python 3.13 "$GPTGREP_DEV_ROOT/eval-venv"
uv pip sync --python "$GPTGREP_DEV_ROOT/eval-venv/bin/python" evals/requirements.lock
"$GPTGREP_DEV_ROOT/eval-venv/bin/python" -B -m unittest discover -s evals -p 'test_*.py'
"$GPTGREP_DEV_ROOT/eval-venv/bin/python" -B scripts/eval.py \
  --binary "$CARGO_TARGET_DIR/release/gptgrep" \
  --output "$GPTGREP_DEV_ROOT/results/offline-eval.json"
```

测试样例覆盖正则匹配正确性、词法检索、Unicode、精确的源文件摘要值与字节区间、
源文件被替换或删除后的过期状态，以及重新索引。原生解析器测试覆盖实际生成的 PDF、书签、
无文本层文档的拒绝处理，以及超过 1,000 页的文档。合并阶段的测试样例会执行固定版本上游的纯函数，
以进行对比。模拟 Codex/HTTP 测试用于验证传输层必须保持的性质；真实宿主或提供方调用的回执属于另一类证据。

外部 PageIndex 数据通过固定版本的清单引用，不随项目重新分发。
在该数据子集上运行 GPTgrep，与执行原版 PageIndex SDK 基线是不同的评估。
上游历史基准数据不属于 GPTgrep 的实测结果。
[基线运行器说明](scripts/pageindex_baseline/README.md)记录了统计分母、消融实验边界、失败案例与费用核算方式。
详细的本地研究和实验解读仍保存在被忽略的 `docs/research/` 路径下。

## 当前版本的限制

- 布局前端由 LiteParse 提供。Rust 的结构与合并阶段不声明与完整 Flash 的字符／字体修复、
  多语言标题分类和生成式扩展算法等价。
- 原生 OCR 已实现，图像组件已有限定范围的 live 验证；完整文档集合 OCR／索引和所有 Office／图像格式的验收仍未完成。缺失或无效的必需结果会明确失败。
- 语义候选预算可能使相关文档或节点未被选中。Jev 无法补回上游已排除的证据；其分数也不保证内容真实。
- 文档集合的新鲜度以快照为准。新增文件需要重新索引；搜索仅重新检查与本次检索相关的源文件，
  而 `status` 会检查现有索引中的全部文档。
- 当前索引操作会重建整个索引版本；增量复用解析结果，以及更广泛的性能与质量基准评估仍是后续工作。

原创代码采用 [MIT 许可证](LICENSE)。依赖项和原生运行库的许可证声明见
[THIRD_PARTY.md](THIRD_PARTY.md)。

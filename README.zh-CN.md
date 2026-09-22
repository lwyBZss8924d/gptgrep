# GPTgrep

[English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

面向智能体的本地文档检索工具：**Rust 文档解析与文档树、内嵌 trigram grep、作为核心的 Jev 路由与重排，以及本地 Codex 推理宿主**。
无需向量数据库、搜索守护进程或 MCP 服务器。

GPTgrep 返回可核查的源文档证据。默认 `search` 使用 `hybrid`，必须执行 Jev 路由与重排。
`ask` 和 `summarize` 也会在最终 Codex 回答 worker 之前执行这一阶段；缺少凭据或提供方调用失败会明确报错。
`semantic` 和 `judge` 同样使用远程 Jev 推理。显式的 `regex` 与 `lexical` 命令是本地检索原语。
推理智能体可以组合搜索、查看文档树，并在回答前分次读取有大小限制的节点内容。

这是开发中的源码。第一个实验版发布前，必须在 PageIndex-OSS-Benchmark 的真实运行中，
证明相对 PageIndex Flash 加 GPT-5.6 的最小优势。请参阅[架构说明](docs/architecture.zh-CN.md)和明确列出的
[Flash 阶段覆盖情况](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md)。
维护者的研究记录保存在 `docs/research/` 下，仅供本地使用，不纳入版本控制。

## 构建与使用

```sh
cargo build --release --locked
./target/release/gptgrep doctor --json
./target/release/gptgrep index ./documents --json
./target/release/gptgrep search 'retention|expiry' ./documents --mode regex --json
./target/release/gptgrep search 'signed snapshot recovery' ./documents --mode lexical --json
```

首次构建时，如果尚未提供所需文件，原生解析器依赖会下载固定版本的 PDFium 二进制文件。
原生发行压缩包包含 PDFium 运行库及其许可证声明；请使用其中的 `gptgrep` 启动脚本来选择随包提供的库。
Office 格式转换还需要 LibreOffice。初始构建关闭了 OCR：没有文本层的扫描文档会明确报错。
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
不会自动降级到纯本地检索。解析和索引发布是确定性的本地准备步骤。
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
网络或响应结构错误会明确报出；不会静默回退、跟随重定向或自动重试。

混合搜索将词法候选与经语义路由选出的树节点结合起来。初始策略最多路由 32 个文档描述，
并对最多 24 个证据候选进行重排。在将结果视为穷尽性结果之前，请先检查 `coverage`。
`--min-score 0.5` 是相关性评分规则的最低门槛，**不是经过校准的置信度**；
`--min-score 0` 会展示低分候选，便于探索。大规模文档集合的分层语义路由仍是明确待完成的扩展性任务。
对于单个完整词元的精确查询，即使 Jev 给出低分，混合搜索的词法路径也会保留经过验证的词元匹配。
结果会显示 `literal_anchor`，以及低于门槛但仍被保留的结果数量；模型分数保持原值。

## 本地 Codex 宿主

[config/codex-host.toml](config/codex-host.toml) 中专用辅助模型的默认配置如下：

```toml
model = "gpt-5.6-luna"
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
  --model gpt-5.6-luna --reasoning-effort max --service-tier fast --json
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


仅 `ask` 支持的 `--experimental-query-plan` 会先运行一个独立、无工具的
`gpt-5.6-luna` / `max` / `fast` 规划 worker。它保留原问题，最多提出两条替代检索表述。
最多两路文档路由同时执行；候选按真实来源跨度合并，仍受原候选预算限制，随后由 Jev
按原问题统一重排，再启动回答 worker。该选项默认关闭，所有阶段共享调用方的总截止时间。
规划或分支失败会明确报错。新增候选可能挤掉原候选并增加延迟，不保证质量提升。
`model_attempts` 和 `model_usage` 单独记录规划调用；旧的 `usage` 仍只表示回答 worker。

```sh
gptgrep ask 'How are offline exports recovered?' ./documents \
  --experimental-query-plan --json
```

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
python3 -m venv .local/eval-venv
.local/eval-venv/bin/python -m pip install --only-binary=:all: -r evals/requirements.lock
.local/eval-venv/bin/python -B -m unittest discover -s evals -p 'test_*.py'
.local/eval-venv/bin/python -B scripts/eval.py --binary target/release/gptgrep --output /tmp/gptgrep-eval.json
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
- 不声明具备 OCR 能力，也不声明支持所有 Office 或图像文件。缺少所需提取能力时会明确失败。
- 语义候选预算可能使相关文档或节点未被选中。Jev 无法补回上游已排除的证据；其分数也不保证内容真实。
- 文档集合的新鲜度以快照为准。新增文件需要重新索引；搜索仅重新检查与本次检索相关的源文件，
  而 `status` 会检查现有索引中的全部文档。
- 当前索引操作会重建整个索引版本；增量复用解析结果，以及更广泛的性能与质量基准评估仍是后续工作。

原创代码采用 [MIT 许可证](LICENSE)。依赖项和原生运行库的许可证声明见
[THIRD_PARTY.md](THIRD_PARTY.md)。

# GPTgrep

[English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

エージェント向けのローカル文書検索ツールです。**文書全体を対象とする Rust の解析、ネイティブ Codex の
解析／OCR エージェント、文書ツリー、組み込みの tgrep 1.1.0、マルチモーダル Decisions のルーティング／
再ランキング、ローカルの Codex 推論ホスト**を組み合わせます。
ベクトルデータベース、検索デーモン、MCP サーバーは使用しません。

GPTgrep は、元文書と照合できる根拠を返します。デフォルトの `search` は `hybrid` を使い、
Jev によるルーティングと再ランキングを必須とします。`ask` と `summarize` も最終回答を作成する Codex reader の
実行前にこの段階を実行し、認証情報の不足やプロバイダーの失敗は明示的なエラーになります。
`semantic` と `judge` もリモートの Jev 推論を使います。明示的な `regex` と `lexical` は
ローカル検索の基本操作です。推論を行うエージェントは、検索を組み合わせ、ツリーを確認し、
上限付きでノードを読み取ってから回答できます。

協力開発の入口は[開発・評価環境](docs/development.md)と日付付きの[実装と live 実行の現状](docs/experimental-status.md)です。
統合ランタイムは、[4 つの Skill と 3 つのネイティブロール](runtime/codex/STAGING.md)、
[ネイティブ操作を網羅する incur CLI](packages/cli/README.md)、
[直接使える Rust API と実行可能な組み合わせ例](docs/rust-sdk.md)を提供します。
Document／OCR の呼び出し側は、ネイティブ機能、権限、ジョブ予算、パッケージのルートを選び、GPTgrep は
選ばれたガイダンスを実際の各作業ディレクトリに配置します。
中核となる最適化目標は、実際の **GPTgrep harness と素の Codex のアブレーション研究**です。62 組の
タスクで同じ Codex reader ランタイムを比較し、INDEX は SEARCH の報酬・コストと分けて報告します。
GPTgrep の reader 契約（基本指示、引用と回答保留の規則、回答規則）は処置（treatment）の一部です。
素の Codex 側は共通のタスクプロンプトだけを受け取り、Codex の既定の基本指示を使います。ソースには
ネイティブパイプラインと full62 Harbor adapter が含まれますが、34 文書全体のネイティブ索引は未受理で、
124 回の対になった SEARCH 試行はまだ実行していません。コンポーネントの検証と処置側のみの開発実行は
この比較を支えるものであり、比較の代わりにはなりません。この Goal は G5 のタグ条件とは別です。

これは開発中のソースで、バージョンは実験的プレリリースの `0.1.0-alpha.1` です。バージョン方針は
[SPEC](SPEC.md)にあります。最初の実験版を公開するには、PageIndex-OSS-Benchmark の実測で、
PageIndex Flash と GPT-5.6 の組み合わせに対する最小限の優位性を確認する必要があります。
元のローカル PageIndex OSS `results.json` は Luna/high で 60/62、別途検証した SDK/Codex の
live 対照は Luna/max で 60/62 です。モデル、バックエンド、証拠の来歴は別々に記録します。
[アーキテクチャ](docs/architecture.ja.md)と、
[Flash の各段階の対応範囲](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md)を参照してください。
`docs/research/` にあるメンテナーの調査ノートはローカル専用で、Git の追跡対象外です。

`gptgrep enrich` はローカル `index` 後に明示的に実行するモデル補助段階です。
`--plan-only` は正規化文書全体を走査して上限付きの作業計画を固定し、モデルを呼びません。
完了した live 構築では Codex（builder の標準モデルは `gpt-6-luna`）と Jev の支持判定で、
ソースに結び付いた独立のナビゲーション層を公開できます。生成したヒントは引用できません。
未完了の作業は台帳に残り、既存の索引は変わりません。文書を限定した計画付き `ask` では、
完了した `enrich` 応答の `artifact_sha256` を `--navigation-overlay-sha256` に渡して明示的に
有効化できます。Jev がソースに結び付いたヒントを全件走査してから計画器と reader
へ渡します。通常の `ask` の動作は維持され、品質とベンチマーク上の効果は live 検証が必要です。

## ビルドと使い方

まず[開発ガイド](docs/development.md)に従い、`CARGO_TARGET_DIR` などの自己管理の出力先を設定してください。

```sh
cargo build --release --locked
"$CARGO_TARGET_DIR/release/gptgrep" doctor --json
"$CARGO_TARGET_DIR/release/gptgrep" index ./documents --json
"$CARGO_TARGET_DIR/release/gptgrep" search 'retention|expiry' ./documents --mode regex --json
"$CARGO_TARGET_DIR/release/gptgrep" search 'signed snapshot recovery' ./documents --mode lexical --json
```

ネイティブパーサーの依存ライブラリは、PDFium バイナリがまだ用意されていない場合、初回の
ビルド時に固定されたバージョンをダウンロードします。ネイティブ版のリリースアーカイブには
PDFium ランタイムとライセンス等の通知文書が含まれます。同梱ライブラリを使うには、アーカイブ内の
`gptgrep` ランチャーを使用してください。Office 形式の変換には、別途 LibreOffice が必要です。
標準の legacy index では OCR は無効です。明示的な rich ingestion と document parse-agent は、
呼び出し側が選択したネイティブ OCR ランタイムを利用します。必須 OCR が不足する場合は、不完全な世代を公開せず失敗します。
プレーンテキストと Markdown は PDFium を読み込まずに処理できます。
検証済みのプラットフォームと再配置の検証については、[リリースのパッケージ化](docs/release.md)を
参照してください。

インデックスは `documents/.gptgrep/` に保存されます。隠しエントリや無視対象の配下、認証情報
ファイル、シンボリックリンク、一般的なビルド用／実行時ディレクトリは除外されます。
完全な再構築に成功するたびに、新しい変更不可の世代を公開します。構築に失敗した場合は以前の
世代を維持します。古い世代は、明示的に管理されるまで保持されます。

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

`--limit` は返す結果の件数を制限します。`--context` のデフォルトは 0 です。
終了ステータスは、成功または一致ありなら 0、一致なしなら 1、エラーまたは古い根拠の除外が
あった場合は 2 です。JSON は標準出力に 1 つのオブジェクトとして出力し、診断情報は標準エラー
出力に送ります。
ノードの読み取りでは `next_offset` が返されます。続きの取得には、その値を `read --offset` に
渡してください。オフセットはノード内の UTF-8 バイト数を数え、各読み取り範囲は正確なソース座標を
保持します。これにより、応答を無制限に大きくせずに、エージェントが大きなセクションを確認できます。

`index --rich`／`--ocr`、`index --parse-agent-config`、または rich core API で公開した v2 世代では、
`gptgrep batch --rich --root ./documents --input rich-operations.json` が
`gptgrep.batch.rich.request.v1` を受け付けます。7 つのローカル操作は `inspect_documents`、
`inspect_pages`、`inspect_native_metadata`、`read_artifact`、`search_artifacts`、`inspect_visual`、
`read_visual` で、`search_multimodal` と `rerank` は明示的な有料操作です。バッチは `CURRENT` を
1 回だけ解決するか、明示的な世代とマニフェスト SHA を使います。入出力はそれぞれ 1 MiB 以内、
操作は最大 64 件で、各項目は実行前に出力領域を予約します。検査系の操作は索引済みのメタデータを返し、
テキストの読み取りと検索は型付きのネイティブ／OCR 根拠とソース検査を保持します。検索カーソルは上限付きの
`max_hits` 結果範囲を対象とするため、呼び出し側は打ち切りと探索の完了を確認する必要があります。
応答は常に JSON で、項目単位または全体の失敗では終了ステータス 2 になります。読み取り側のバイト数は
API による部分的な読み取りを表し、物理 I/O の総量ではありません。明示的な `search_multimodal` は、
同じ保持済みの読み取り側と有限の Decisions コンテキストで、必須の文書ルーティングとテキスト／画像の
混合再ランキングにも対応します。出力と呼び出しの割り当てはすべて送信前に受け入れ判定され、有料の
カーソルやフォールバックはありません。`budget.max_decisions_submissions` の既定値は 8（上限 200）で、
親の割り当てをリセットしません。オプション、順序付きの失敗、上限付きの計測と観測の限界は
[rich batch の組み合わせ](docs/rich-batch.md)を参照してください。

`gptgrep rerank --root ROOT --input candidates.json --json` は、固定した 1 つの rich 世代に属する、
呼び出し側が指定したネイティブ／OCR テキストまたは画像根拠の参照だけを採点します。内容は保存済みの
成果物から解決し、呼び出し側が渡すテキスト、画像バイト、URL は拒否します。rich batch の同等の
`rerank` 操作は、ローカル操作や `search_multimodal` と固定の出力・呼び出し予約を共有します。
明示的な rerank は文書ルーティングを行いませんが、通常の検索は必須のルーティングを維持します。
どちらの rerank インターフェースも reader への提供権限や引用権限を与えません。厳密な入力と結果の契約は
[ソースに結び付いた rerank](docs/rerank.md)を参照してください。
デフォルトの `index` は元の形式で公開します。明示的な `--rich` / `--ocr` は rich ingestion を選び、
`--parse-agent-config` と完全な文書制御レジストリはネイティブなエージェント解析と自己管理の成果物の
取り込みを選びます。これらは実行時要件が明示された別々のインターフェースです。[開発ガイド](docs/development.md)
を参照してください。parse-agent の画像ファイル、base64、ページごとのバイト容量は、制御ファイルの
`resources` オブジェクトで設定します。連動する上限、ネイティブ profile の結び付け、保持される固定画像の
契約は [4 MiB 画像の例](docs/architecture.ja.md#ネイティブ解析と-ocr)を参照してください。

Rust コアは、`VisualGenerationReader` の rendition から得た PNG の切り抜き用に `DerivedVisualCache`
を提供します。各派生画像は、世代、ソース、親画像、切り抜きレシピ、座標変換、エンコーダー、出力
ダイジェストに結び付きます。キャッシュのヒット時と読み取り時には親画像を再検査します。payload、
メタデータ、作業領域の容量は別々に受け入れ判定し、保持中の画像 pin とレシピ token は、追い出しや
`clear` の後も容量の割り当てを維持します。このプロセス内キャッシュは封印された世代を変えず、reader への
画像提供権限や引用権限も与えません。上限の対象は自己管理のキャッシュ payload と切り抜きの受け入れ判定であり、
プロセス全体のメモリではありません。

## Jev による中核検索

必須の Decisions 段階の既定モデルは `openai/gpt-6-luna-decisions` です。明示的な
`--model typesafe/jev-1.13` はテキスト専用の旧 Jev を選びます。デフォルトの検索では文書ルーティングと
根拠の再ランキングを実行し、ローカルのみの検索へ自動的に切り替えることはありません。
標準解析はローカルですが、明示的な OCR、document-agent、enrich は INDEX 中にモデルを呼びます。
公開には完全で検証済みの成果物が必要です。
`--document manual.pdf` を指定すると、候補の上限を適用する前に、インデックス内の正確な
パス 1 件へ検索を絞ります。結果は `document_scope` と `coverage` を示します。

既存のシークレット管理ツールを使い、プロセスの環境変数に `OPENROUTER_API_KEY` を設定して
ください。GPTgrep が認証情報ファイルを自動的に読み込むことはありません。開発用の任意ヘルパー
`scripts/with_dev_key.py` は、明示的に選択されたエントリだけを読み取ります。シェルコードの
実行、env ファイルのコピー、値の表示は行いません。

```sh
gptgrep search 'how can a damaged journal be recovered?' ./documents \
  --model typesafe/jev-1.13 --min-score 0.5 --json

gptgrep judge --input evals/requests/decision-smoke.json \
  --model '~typesafe/jev-latest' --json
```

応答は実際に返されたモデルのメタデータを保持します。受け入れには、要求した識別子、またはその観測済みの
正規ビルド（Luna は `openai/gpt-6-luna-decisions-20261006`、旧 Jev は `typesafe/jev-1.13-20260917`）
が必要です。その他の明示的なモデルは識別子の完全一致が必要です。推測した日付付き識別子、別のファミリー、
フォールバックのモデルは受け入れません。ネイティブ Codex の推論は `gpt-6-luna`、`max`、`fast` を別に使います。

`~typesafe/jev-latest` は OpenRouter のエイリアスです。カタログが Jev ファミリーの最新ビルドに解決し、
応答には実際に回答したビルドが返ります。GPTgrep が `~author/family-latest` エイリアスを受け入れるのは、
応答したモデルが同じ作者・同じファミリーの具体的なビルド（数字で始まるバージョンを持つ
`author/family-VERSION`）である場合だけです。このエイリアスでは `typesafe/jev-1.13` と
`typesafe/jev-1.13-20260917` を受け入れ、別の作者、`typesafe/jevx-2` のような別のファミリー、`:free`
の派生、別のエイリアスは拒否します。この拒否は呼び出しが課金された後に起こります。形式が不正なエイリアスは
リクエスト前に拒否します。エイリアスはテキスト専用で、型付き画像と生の画像パートはリクエスト前に拒否します。
報告では要求したセレクターと応答したモデルを分けて保持し、judge の出力は応答した `model` と並べて
`requested_model` を報告します。結果の比較可能性を保つ必要がある場合は、正規ビルドを固定してください。

Jev はチャット補完ではなく、OpenRouter の型付き **Decisions API** を使用します。
動的な Choice/Noul/Score の判断を提供し、解析、オフセット、算術処理、ワークフローの実行は
アプリケーションコードが担います。応答には、実際に返されたモデル名と、取得できた使用量・
コストを記録します。不明な指標は null のままです。ネットワークやスキーマのエラーは明示し、
通知なしのフォールバックやリダイレクトは行いません。既定の検索のルーティング／リランクは、
HTTP 429/500/502/503/504 を受信した場合に限り、元のリクエスト期限と提出枠の範囲で最大1回だけ
再試行します。失敗した試行は不明なコストのまま報告します。再試行の前には 250〜1000 ms の
ジッター付き待機、またはそれより長い整数秒の `Retry-After` を待ちます。それ以外の形式の
`Retry-After`（HTTP 日付、重複したフィールド、負数・小数・10 桁を超える値）は再試行を拒否し、
`retry_after_refusal` として記録します。再試行を送るのは、元の期限にその待機と 2 秒が残り、
提出枠も残っている場合だけです。タイムアウトなどその他の失敗は再試行しません。

ハイブリッド検索は、語句による候補と、意味に基づくルーティングで選ばれたツリーノードを
組み合わせます。初期の方針では、最大 32 件の文書説明をルーティング対象とし、最大 24 件の
根拠候補を再ランキングします。ルーティング可能な文書が 1 件だけの場合（`--document` の範囲
など）は、ルーティング呼び出しを行わずにその文書を選びます（`coverage.routing_status`）。
結果を網羅的なものとして扱う前に、`coverage` を確認してください。`--min-score 0.5` は関連性の
評価基準における下限であり、**校正済みの信頼度ではありません**。探索時に低スコアの候補も表示するには
`--min-score 0` を指定します。大規模な文書集合に対する階層的な意味ルーティングは、明示された今後の拡張課題です。
1 トークンを厳密に指定したクエリでは、Jev のスコアが低くても、ハイブリッド検索の語句検索経路が
検証済みのトークン一致を保持します。結果には `literal_anchor` と、下限未満でも保持された件数が
示されます。モデルのスコア自体は変更しません。下限に届く候補がない場合も、スコア上位 3 件を
低信頼度の候補として `below_threshold: true` を付けて返し、`coverage.below_threshold_fallback`
と警告で示します。これらが下限を満たしたとは扱いません。結果を返すため終了ステータスは 0 です。
プレーンテキスト出力ではこれらを `PATH:LINE:TEXT` の一致ではなく grep の文脈行
（`PATH-LINE-TEXT`）として出力するので、`--json` 出力の `below_threshold` を確認してください。

## Decisions の judge と型付き画像

`gptgrep judge --input evidence.json --json` は通常の型付き画像契約も受け付けます。`input.text` には
`{source, text}` オブジェクト、`input.images` にはローカルの PNG/JPEG ファイルを指す `{source, path}`
オブジェクトを `questions` と並べて入れます。各 `source` は `source_id`、`unit_id`、`source_sha256`
を含みます。ソースのダイジェストは呼び出し側が示す来歴であり、独立した鮮度の主張ではありません。
テキストは文字どおりに扱い、画像バイトは上限付きのスナップショットとして受け入れ、順番どおりにフラットな
`image_url` の data URL として送ります。ソースの結び付けとローカルパスはプロバイダーへの本文に含めません。
64 KiB の制御／テキストのエンベロープ、画像 4 枚、1 枚あたり 1 MiB／800 万画素、合計 2400 万画素、
リクエスト 8 MiB、応答 1 MiB の上限を明示的に適用します。旧 Jev、その他の明示的なモデル、すべての
エイリアスは、予約や送信の前に型付き画像を拒否します。生の `{state, questions}` 入力にも同じ規則を適用し、
これらのセレクターでは `state` や `questions` 内の、`image_url` または `input_image` のメンバーや `type`
を持つオブジェクトを予約前に拒否します。文字列は文字どおりのテキストのままです。Luna には生の JSON を
そのまま渡します。拒否、不明な回答、不完全な本文、形式が不正な応答は、成功した判定になりません。

judge が成功すると検証済みの応答を出力し、`model` は応答したビルドで、`requested_model` も併記します。
失敗した judge は終了ステータス 2 で `gptgrep.error.v1` を返し、コードは `decisions_judge_failed`、
`judge` オブジェクト（`gptgrep.judge-failure.v1`）を含みます。このオブジェクトには、段階、要求したモデルと
応答したモデル、プロバイダーの応答 ID が入ります。送信が始まったかどうか（`not_submitted`、
`sent_response_received`、`sent_outcome_unknown`）も記録します。使用量と `cost_usd` は取得できた場合に
示し、取得できない場合は `not_submitted`、`http_status_body_not_read`、`cost_null` などの理由付きで
null になります。レイテンシ、失敗の診断情報、コンテキストの受け入れ判定のスナップショットも含みます。
リクエストや応答の本文、ヘッダー、URL、キーは決して含みません。

## 共有の Decisions 予算

共有の実験では、hybrid/semantic 検索（`--multimodal` を含む）、judge、ask、summarize、plan-only 以外の
enrich、明示的な `rerank`、有料の `batch --rich` 操作に `--decisions-budget budget.json` を加えます。
可搬性のある制御ファイルは 16 KiB 以内で、`schema_version: "gptgrep.decisions-budget-contract.v1"`、
`run_id`、`plan_digest`、`deadline_unix_ms`、`max_submissions`、絶対パスの `ledger_path` を含みます。
このオプションは既存の一致する台帳に参加し、その元の時計と消費済みの予約を引き継ぎます。ファイルの欠落や、
run、期限、計画、上限の変更はエラーとなり、台帳を作成やリセットすることはありません。新しい台帳は一度だけ
作成します。

```sh
gptgrep decisions-budget init --run-id smoke-1 --plan-file plan.json \
  --deadline-seconds 1800 --max-submissions 2 \
  --ledger /abs/private/ledger.json --output /abs/private/budget.json
gptgrep decisions-budget status --control /abs/private/budget.json --json
```

`init` は `plan_digest` を計画ファイル（1 MiB 以内の通常ファイル）の SHA-256 に、期限を現在時刻 + N 秒
（1〜86,400、延長なし）に設定します。`--max-submissions` は 1〜200 を受け付けます。時計のアンカーは
作成時の起動と時計の名前空間に結び付くため、再起動後の参加は安全側に失敗します。元の時計アンカーとともに
ジャーナルとその `.lock` 補助ファイルを作成し、制御ファイルをモード 0600 で一度だけ書き込みます。相対パスと、
既存の台帳、ロック、制御ファイルのパスは拒否し、参加、リセット、延長、置き換えは行いません。`status` は
読み取り専用で、結果別（`reserved`、`in_flight`、`unknown`、`terminal`）の予約、残りの提出数、要求した
セレクター、期限の状態、受け入れが開いているかを報告し、期限後も読み取れます。予約は取り消されません。
参加した実行は `max_submissions` に達すると、POST の前に `Decisions submission budget exhausted` で
停止します。ルーティング、再ランキング、ホストの後続クライアントは同じコンテキストを共有します。
オプションがない場合は、上限付きの単一呼び出しコンテキストを使います。regex/lexical、ローカルのみの
バッチ、schema、doctor、enrich `--plan-only` は予算ファイルを開きません。`parse-agent`、`ocr serve`、
`index --parse-agent-config`、`decisions-budget` サブコマンドは、このグローバルオプションを拒否します。
数値の使用量とコストはプロバイダーが報告したメタデータのままで、欠けた項目は取得不可のままです。台帳が
数えるのは提出数であり、USD ではありません。支出上限は製品の外側で、たとえば提出数に最悪時の 1 回あたり
価格を掛けて管理します。

## ローカル Codex ホスト

専用ヘルパーのデフォルト設定は、[config/codex-host.toml](config/codex-host.toml)で
次のように定義されています。

```toml
model = "gpt-6-luna"
model_reasoning_effort = "max"
service_tier = "fast"
approval_policy = "never"
sandbox_mode = "read-only"
```

インストール済みの Codex と、認証済みのローカルアカウントが必要です。高度な Document／OCR 経路は、
固定した単体の app-server とソースに結び付いた制御ファイルを選びます。保持される固定画像の profile は
[app-server の派生版](patches/codex-native-viewer/README.md)と、それぞれ別の検証を使います。
通常の CLI はデバイスログインと選択された reader ワークフローを担当します。設定の選び方は
[開発ガイド](docs/development.md)で説明しています。開発環境を分離する場合は、使用するアカウントの
ホームを明示的に選択してください。認証は Codex が担当します。GPTgrep は認証キャッシュの読み取りや
コピー、グローバルプロファイルのインストールを行いません。まだログインしていない環境でのヘッドレスな
デバイスコード認証については、[Codex の認証ドキュメント](https://learn.chatgpt.com/docs/auth?surface=cli)を
参照してください。

```sh
gptgrep ask 'What is the recovery procedure, and how long are snapshots kept?' \
  ./documents --json

gptgrep summarize DOCUMENT_ID:NODE_ID --root ./documents \
  --model gpt-6-luna --reasoning-effort max --service-tier fast --json
```

デフォルトでは reader の起動前に、ホストは必須の Jev ハイブリッド検索を実行し、上限付きの根拠を
推論モデルに渡します。`--jev-model` は Decisions モデルを選び、Codex の `--model` とは
独立した設定です。`--document` で質問の対象を絞り、要約では選択したノードの文書を対象にします。
後続の検索もデフォルトは hybrid です。ツリーの読み取りと明示的な厳密検索は、初期結果を
補う操作として利用できます。報告には、Codex の使用量と併せて、Jev の処理範囲、実際のモデル、
使用量も保持します。

テキスト検索の reader（制限付きとネイティブのどちらも）には汎用の回答規則も与えます。質問が求める形で
直接の答えを先に示すこと、読んだ根拠が質問どおりの問いに答えている場合はその答えを確定させて引用すること、
本文中の参照（注記号、引用番号、「表／節を参照」などの指示）を参照先の箇所を読んで解決することを求めます。
これらの規則は根拠、引用、`insufficient_evidence` の規則を緩めません。リッチテキストとマルチモーダルの
reader には与えません。これらは GPTgrep の reader 契約の一部です。

デフォルトの制限付きモードでは、ホストは実行環境を空にした一時的な stdio app-server スレッドを作成し、
上限を設けた GPTgrep の catalog/tree/search/read ツールを提供します。実際に適用されたサンドボックスと
承認設定を検証し、時間とツール呼び出し回数を制限し、想定外のサーバー要求を拒否し、自身が
起動した子プロセスを終了して回収します。モデルが記述した引用は、その実行中に実際に提供された
根拠を参照し、かつ現在のソースと一致していなければなりません。レポートでは、回答、根拠、
ツール実行記録、実際のモデル／推論強度、使用量を分けて報告します。
ホストは `.gptgrep/host-attempts/` に、非公開かつサイズ制限のあるメタデータ台帳も書き込みます。
後続の段階が失敗または中断しても、完了した Jev 呼び出しを確認できるようにするためです。
ソース文書は変更せず、Codex 子プロセスには Jev の認証情報を渡しません。
引用の同一性の検査だけで、
根拠が回答内容を意味的に裏付けることまで独立に証明できるわけではありません。

`ask --native-ptc --codex-permissions PROFILE` は、既存の名前付き Codex 権限 profile のもとで、
最終の検索 reader をネイティブ実行で動かします。ランタイムはターンの前にその profile を確認する必要があり、
旧来のサンドボックス上書きは省略します。reader は選択された Codex の機能と Skill を継承し、シェルと
画像表示を有効にします。観測されたネイティブ項目と受け付けた GPTgrep の呼び出しは
`--max-tool-calls`（既定 12）を共有します。予算を使い切った後の有効な GPTgrep の要求には、
制限付き reader と同じ拒否（後述）を返します。拒否した要求は新しい根拠を発行せず、Jev も実行せず、
共有の計数にも加えないため、reader は同じターンと元の期限内で、回答または `insufficient_evidence` を
返して完了できます。この拒否が `--max-tool-calls` 回に達すると、次の要求で `ask` は
`host_native_ptc_tool_budget_exhausted` で失敗し、回答を返しません。`ask --multimodal --native-ptc`
にはこの拒否がなく、予算を超えた最初の GPTgrep の要求が同じコードで失敗します。ネイティブ項目の通知は
実行開始後に届くことがあるため、ネイティブ項目は拒否できません。
この予算は計数であって実行前の防護ではなく、予算を超えて観測されたネイティブ項目は同じコードで
`ask` を失敗させます。最初の拒否の後、`model_attempts[].native_ptc.tool_budget` は制限付き reader と
同じ診断として、上限、受け付けた呼び出し数（ネイティブ項目と GPTgrep の呼び出しの合計で、上限と等しい）、
拒否数、拒否の上限を報告します。内訳は `dynamic_requests_charged` と `native_tool_calls` に残ります。
制限付き reader の `--timeout` 900 秒と `--max-tool-calls` 64 回の上限は、このモードには適用されません。
ネイティブ出力は引用を生まず、回答は GPTgrep が実際に提供した根拠を引用する必要があります。クエリ計画器と
`host-complete` はツールを使いません。

`ask` 専用の `--experimental-query-plan` は、ツールを持たない独立した
`gpt-6-luna` / 呼び出し側が選んだ推論強度 / `fast` の計画器を標準で先に実行します
（推論強度の標準値は引き続き `max`）。
`--planner-model` で別のモデルを明示でき、比較実験では `gpt-5.6-luna` を選べます。元の質問を保持し、
検索表現を最大2件追加します。ルーティングは最大2件を並行実行し、同じソース範囲を
重複除去して既存の候補数上限内にまとめます。その後、Jev が元の質問に対して再評価し、
reader を開始します。このオプションは初期状態で無効で、全段階が呼び出し元の
期限を共有します。計画や分岐の失敗は明示されます。追加候補が元の候補を押し出したり、
待ち時間を増やしたりする場合があり、品質向上は保証しません。`model_attempts` と
`model_usage` は計画呼び出しを個別に記録し、従来の `usage` は reader のみを表します。

```sh
gptgrep ask 'How are offline exports recovered?' ./documents \
  --experimental-query-plan --json

# enrich 完了後、その artifact_sha256 と正確な文書パスを渡します。
gptgrep ask 'How are offline exports recovered?' ./documents \
  --document manual.pdf --experimental-query-plan \
  --navigation-overlay-sha256 "$OVERLAY_SHA" --json
```

上のコマンドに `--experimental-evidence-roles` を追加すると、初期状態で無効の
`jev-evidence-role-v1` 選択方針を試せます。Jev は関連性 Score を保持し、候補ごとに
直接的な裏付け、局所的に不完全な根拠、背景、裏付けなしの4択 Choice を追加します。
元の literal anchor の優先順位、スコアの下限、初期結果3件の上限は維持します。
役割による順序と厳密な範囲に結び付いたヒントは、根拠の選択と周辺ソースの追加読取を
助けます。返された根拠集合の十分性は未評価です。1つの役割が質問の一部を裏付けても、
質問全体を満たすとは限りません。出力調整で評価済みのバイト範囲が変わった場合、その
役割ヒントは利用不可になります。除外候補の診断には本文を含めず、引用権限も与えません。

最大24候補に対して同じ統合 Decisions リクエスト内で最大48問を送ります。完全にエンコード
したリクエスト本文は64 KiB以内でなければなりません。追加判断によって使用量や遅延が
増える可能性があり、その後の reader の処理も引き続き予算内で計測します。このオプション
自体は初期 Jev 操作や Codex ステップを追加しません。`--experimental-query-plan` が必要で、
失敗時のフォールバックは有効にしません。

`--experimental-source-continuation`（初期状態で無効。`--experimental-query-plan` と `--document`
が必要）は reader だけに指示を加えます。`next_offset = null` はノードの終わりであって調査の終わりでは
ないため、reader は既存のツール・時間・出力の予算内で、範囲を限定したツリー／読み取りノードを使って
未解決の主張を確認し、確認できなければ `insufficient_evidence` を返します。モデル呼び出しは増えず、
発行済みの範囲を超える引用権限も与えず、品質への効果は未測定です。

デフォルトの制限付き reader では、検索ツールの予算（`--max-tool-calls`、既定 12）に達すると、以降の要求は
拒否され、同じ Codex ターンと元の期限内で回答を完了するよう指示されます。拒否した要求は新しい根拠を
発行せず、Jev も実行しません。その診断件数はモデルのターン数と分けて記録します。拒否した要求も予算に
達すると、次の要求で `ask` は `host_tool_budget_exhausted`（終了ステータス 2）で失敗し、回答を返しません。
`node_coverage` は返された各断片の範囲であり、累積の読取範囲ではありません。EOF に達しても、その断片に
前方の内容が含まれない場合があります。

`host-complete --input FILE_OR_DASH` は、同じ分離されたローカルモデルを、型付きワークフローの
構成要素として提供します。入力は `{instructions, state, schema}` で、出力にはスキーマ検証済みの
`value`、モデル／推論強度、使用量、入力ダイジェストが含まれます。入力の合計上限はデフォルトで
256 KiB であり、明示的に最大 1 MiB まで変更できます。プロンプトを通知なしに切り詰めることは
ありません。この補完モードに引用検証の保証はありません。比較評価で元の PageIndex の要約・
最適化プロンプトを実行するために定義された、プロバイダーブリッジに対応しています。

app-server API は実験段階です。対応範囲と設定については、
[ホストクレートのドキュメント](crates/gptgrep-host/README.md)に記録されています。

## マルチモーダル検索

`search --multimodal` と `ask --multimodal` は、v2 の rich 世代を通常の Decisions とネイティブ reader の
ワークフローにつなぎます。

```sh
gptgrep search 'What does the chart show?' ./documents --multimodal --max-visual-candidates 8 --json
gptgrep ask 'Explain the chart and supporting text.' ./documents --multimodal --document report.pdf --max-citations 4 --json
```

どちらのコマンドも `CURRENT` を 1 回だけ解決し、ソースに結び付いたその世代と有限の Decisions 実行
コンテキストを 1 つ保持し、必須の文書ルーティングの後、テキスト候補と独立した画像候補を再ランキングします。
画像の経路には、クエリと文字どおりに一致しないページも含まれます。既定の Decisions モデルは
`openai/gpt-6-luna-decisions` で、マルチモーダル検索は観測済みの正規識別子
`openai/gpt-6-luna-decisions-20261006` も受け入れます。マルチモーダルの ask には別のネイティブ
`gpt-6-luna/max/fast` profile が必要です。明示的な旧 Jev は引き続きテキストのワークフローで使えます。

マルチモーダルの出力は、`--json` を省略しても常に 1 つの型付き JSON レポートです。検索は
`gptgrep.rich_multimodal_search.v1` を返し、その画像の提示は引用権限を与えません。ask は
`gptgrep.rich_multimodal.host.v1` を返し、初期検索のレポートを保持します。テキストと画像の引用には、
ネイティブ reader への完全な実際の提供と、その後のソース、ハッシュ、座標の検査が必要です。部分的な失敗は、
観測した検索・計量情報を既存の `gptgrep.error.v1` エンベロープに保持し、終了ステータス 2 になります。

上限は `--window-bytes`（4〜2048）、`--max-visual-candidates`（1〜16）、`--max-concurrent-batches`（1〜4）、
`--max-image-bytes`（最大 1 MiB）、`--max-image-pixels`（最大 800 万）、`--max-metadata-bytes`
（最大 512 KiB）、`--max-output-bytes`（最大 1 MiB）で設定できます。ask は 1〜8 の `--max-citations`
も受け付け、ホストの候補メタデータ 32 KiB と初期レポート 512 KiB の上限を適用します。予算が厳しすぎると、
モデル呼び出しの前に準備を拒否することがあります。レポートは、調べた候補と省いた候補、打ち切り、
不完全な意味的カバレッジを区別します。これらの上限は自己管理のバッファーと通信を表すもので、
プロセス全体の RSS や網羅的な再現率を保証するものではありません。

`--multimodal-timeout` は 1〜900 秒を受け付け、検索の既定は 180 秒です。ask はこの値と既存の
`--timeout` の小さい方を使います。`--decisions-budget FILE` は、ルーティング、再ランキング、ホストの
Decisions ツールについて、同じ既存の元時計の共有コンテキストに参加し、台帳の初期化やリセットは行いません。
このファイルがない場合、コマンドは有限の単一呼び出しコンテキストを 1 つ作成し、その計画の識別情報は
保持した世代のマニフェストに結び付きます。可搬性のあるキャンペーン予算を意味するものではありません。

v1 世代、regex/lexical/semantic のモード選択、旧来の grep／範囲／候補オプション、ask の実験的な
計画／ナビゲーション／続き読みオプションは、このワークフローでは拒否します。テキストや regex への
フォールバックはありません。`--multimodal` を省略すると、従来の経路と出力を維持します。rich ingestion の
入口は、損失のない UTF-8 テキスト／Markdown、PDF、PNG、制限付きの厳格な baseline JPEG に対応します。
プレーンテキストは元のバイトと行範囲を保持し、物理ページや OCR を主張しません。Office 入力は上限付きの
LibreOffice 変換を 1 回だけ行い、その結果を解析、描画、OCR で共有します。動作する変換器が必要です。
範囲を限定した既存の Office の根拠は、すべての Office 形式や文書への対応を示すものではありません。
JPEG の他の方式は引き続き対象外です。CLI の組み合わせとオフラインのモック検査が示すのは実装の振る舞いです。
範囲を限定したネイティブ画像の検証は、文書集合全体のネイティブ索引、現行ソースでのプロバイダーの結果、
まだ完了していない対比較とリリース条件とは別です。

## フォアグラウンド OCR

フォアグラウンド OCR は、ネイティブの `gptgrep ocr serve --config CONTROL_JSON --token-env GPTGREP_OCR_SERVE_TOKEN`
入口が提供します。任意の incur ラッパーの `ocr serve` は、同じネイティブプロセスをフォアグラウンドで監督する
もので（POSIX のみ）、2 つ目のサービスは追加しません。認証付きのループバック、サーバー側が明示的に保持する
ソース／タスクの管理、有限の元アンカーのフェーズを使います。タスク全体と PNG の LiteParse 互換モードは
それぞれ別の検証と来歴を持ち、互換モードの信頼度はモデルの報告値で、校正されていません。制御スキーマ、
作成専用の台帳の動作、上限、未完了のランタイム検証は[フォアグラウンド OCR](docs/ocr-serve.md)を参照してください。
通常のクエリが OCR を起動することはありません。

## 任意の incur インターフェース

grep インターフェースの本体はネイティブバイナリです。`packages/cli` は、実際の incur による
スキーマと検出のインターフェースを `gptgrep-ai` として追加し、シェルの文字列展開を介さずに、
型付きリクエストオブジェクトをネイティブ実行ファイルへ渡します。このラッパーでは、MCP サーバー、
スキル同期、更新のエントリーポイントを無効にしています。

```sh
cd packages/cli
pnpm install --frozen-lockfile
GPTGREP_BIN=/absolute/path/to/gptgrep node src/cli.js --schema
GPTGREP_BIN=/absolute/path/to/gptgrep node src/cli.js search \
  --request '{"query":"retention","root":"/absolute/path/to/documents","mode":"lexical"}'
```

正確なリクエストキーは、[ラッパーのドキュメント](packages/cli/README.md)を参照してください。
ラッパーは公開済みの incur 0.5.1 に固定しています。調査対象のローカルな 0.6.0 チェックアウトは、
この調査時点ではレジストリに公開されていませんでした。

## 検証と根拠

これらの検査の前に、[開発ガイド](docs/development.md)に従って明示的な外部出力パスと uv 環境を設定して
ください。Python の検査には 3.12 以上とリポジトリのロックファイルを使います。

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

フィクスチャによるテストは、正規表現の正しさ、語句検索、Unicode、ソースの正確なダイジェストと
バイト範囲、更新／削除による古い状態の検出、再インデックス化を対象とします。ネイティブパーサーの
テストには、実際に生成した PDF、ブックマーク、テキストのない文書の拒否、1,000 ページを超える
文書が含まれます。マージ段階のフィクスチャは、比較のために固定リビジョンの上流の純粋関数を
実行します。Codex/HTTP のモックテストが確かめるのは通信経路の不変条件であり、実際のホストや
プロバイダーの実行記録は別の根拠として扱います。

外部の PageIndex データは固定マニフェストで参照し、再配布しません。その一部に対して GPTgrep を
実行することと、元の PageIndex SDK によるベースラインを実行することは別です。上流で過去に
報告されたベンチマーク値は、GPTgrep の測定値ではありません。評価の分母、要素を除外した比較の
範囲、失敗事例、コスト集計については、[ベースラインランナー](scripts/pageindex_baseline/README.md)を
参照してください。詳しいローカル調査と実験結果の解釈は、無視対象の `docs/research/` 配下に保持します。

## このリリースの制限

- レイアウト解析のフロントエンドには LiteParse を使用します。Rust の構造化／マージ段階について、
  Flash の完全な文字・フォント修復、多言語の見出し分類、生成モデルによる拡張との同等性は主張しません。
- ネイティブ OCR は実装済みで、画像コンポーネントには範囲を限定した live の根拠があります。ネイティブ
  組み込みツール、シェル、画像・CV 処理を含む解析／OCR の完全なネイティブ PTC コーディングは進行中で、
  固定 Page/Document ガードの検証は個々のコンポーネントだけを対象とします。文書集合全体の OCR／索引と、
  すべての Office／画像形式の受理は未完了です。必要な抽出の不足や不正は明示的な失敗として扱います。
- 既定（v1）の索引は、対応形式の文書に抽出可能なテキストが 1 つもない場合（スキャンした PDF など）に
  構築全体を拒否し、以前の世代を現行のまま残します。そうした文書集合には `--ocr auto` による明示的な
  rich ingestion か parse-agent による索引が必要です。
- reader の「答えを確定させる」規則は評価中です。この規則を導入した Opt-1 の変更の開発測定では、失敗した
  実行は減りましたが、回答不能な質問に答える件数が増えました。[実験の現状](docs/experimental-status.md)を
  参照してください。
- 意味検索の候補数には上限があり、関連する文書やノードを取りこぼす可能性があります。Jev は前段で
  除外された根拠を取り戻せず、そのスコアも真実性の保証にはなりません。
- 文書集合の鮮度はスナップショットの契約に基づきます。新しいファイルを追加したら、インデックスを
  再構築してください。検索はその取得に関係するソースだけを再検査し、`status` は既存のインデックス済み
  文書をすべて検査します。
- 現在のインデックス化は世代全体を再構築します。解析結果の差分再利用と、幅広い性能・品質ベンチマークは
  今後の課題です。

独自に開発したコードは [MIT ライセンス](LICENSE)で提供します。依存ライブラリとネイティブ
ランタイムの通知文書については、[THIRD_PARTY.md](THIRD_PARTY.md)を参照してください。

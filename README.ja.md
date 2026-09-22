# GPTgrep

[English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

エージェント向けのローカル文書検索ツールです。**Rust による解析と文書ツリー、組み込みの
トライグラム grep、中核となる Jev のルーティング／再ランキング、ローカルの Codex
推論ホスト**を組み合わせます。ベクトルデータベース、検索デーモン、MCP サーバーは使用しません。

GPTgrep は、元文書と照合できる根拠を返します。デフォルトの `search` は `hybrid` を使い、
Jev によるルーティングと再ランキングを必須とします。`ask` と `summarize` も最終回答を作成する Codex reader の
実行前にこの段階を実行し、認証情報の不足やプロバイダーの失敗は明示的なエラーになります。
`semantic` と `judge` もリモートの Jev 推論を使います。明示的な `regex` と `lexical` は
ローカル検索の基本操作です。推論を行うエージェントは、検索を組み合わせ、ツリーを確認し、
上限付きでノードを読み取ってから回答できます。

これは開発中のソースです。最初の実験版を公開するには、PageIndex-OSS-Benchmark の実測で、
PageIndex Flash と GPT-5.6 の組み合わせに対する最小限の優位性を確認する必要があります。
[アーキテクチャ](docs/architecture.ja.md)と、
[Flash の各段階の対応範囲](crates/gptgrep-pageindex/FLASH_STAGE_COVERAGE.md)を参照してください。
`docs/research/` にあるメンテナーの調査ノートはローカル専用で、Git の追跡対象外です。

## ビルドと使い方

```sh
cargo build --release --locked
./target/release/gptgrep doctor --json
./target/release/gptgrep index ./documents --json
./target/release/gptgrep search 'retention|expiry' ./documents --mode regex --json
./target/release/gptgrep search 'signed snapshot recovery' ./documents --mode lexical --json
```

ネイティブパーサーの依存ライブラリは、PDFium バイナリがまだ用意されていない場合、初回の
ビルド時に固定されたバージョンをダウンロードします。ネイティブ版のリリースアーカイブには
PDFium ランタイムとライセンス等の通知文書が含まれます。同梱ライブラリを使うには、アーカイブ内の
`gptgrep` ランチャーを使用してください。Office 形式の変換には、別途 LibreOffice が必要です。
初期ビルドでは OCR は無効で、テキストのないスキャン文書は明示的なエラーになります。
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

## Jev による中核検索

Jev は GPTgrep 検索システムの必須要素です。デフォルトの検索では文書ルーティングと根拠の
再ランキングを実行し、ローカルのみの検索へ自動的に切り替えることはありません。
解析とインデックスの公開は、決定的なローカルの準備処理です。
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

Jev はチャット補完ではなく、OpenRouter の型付き **Decisions API** を使用します。
動的な Choice/Noul/Score の判断を提供し、解析、オフセット、算術処理、ワークフローの実行は
アプリケーションコードが担います。応答には、実際に返されたモデル名と、取得できた使用量・
コストを記録します。不明な指標は null のままです。ネットワークやスキーマのエラーは明示し、
通知なしのフォールバック、リダイレクト、自動再試行は行いません。

ハイブリッド検索は、語句による候補と、意味に基づくルーティングで選ばれたツリーノードを
組み合わせます。初期の方針では、最大 32 件の文書説明をルーティング対象とし、最大 24 件の
根拠候補を再ランキングします。結果を網羅的なものとして扱う前に、`coverage` を確認して
ください。`--min-score 0.5` は関連性の評価基準における下限であり、**校正済みの信頼度では
ありません**。探索時に低スコアの候補も表示するには `--min-score 0` を指定します。
大規模な文書集合に対する階層的な意味ルーティングは、明示された今後の拡張課題です。
1 トークンを厳密に指定したクエリでは、Jev のスコアが低くても、ハイブリッド検索の語句検索経路が
検証済みのトークン一致を保持します。結果には `literal_anchor` と、下限未満でも保持された件数が
示されます。モデルのスコア自体は変更しません。

## ローカル Codex ホスト

専用ヘルパーのデフォルト設定は、[config/codex-host.toml](config/codex-host.toml)で
次のように定義されています。

```toml
model = "gpt-5.6-luna"
model_reasoning_effort = "max"
service_tier = "fast"
approval_policy = "never"
sandbox_mode = "read-only"
```

インストール済みの Codex と、認証済みのローカルアカウントが必要です。開発環境を分離する場合は、
使用するアカウントのホームを明示的に選択してください。認証は Codex が担当します。GPTgrep は
認証キャッシュの読み取りやコピー、グローバルプロファイルのインストールを行いません。
まだログインしていない環境でのヘッドレスなデバイスコード認証については、
[Codex の認証ドキュメント](https://learn.chatgpt.com/docs/auth?surface=cli)を参照してください。

```sh
gptgrep ask 'What is the recovery procedure, and how long are snapshots kept?' \
  ./documents --json

gptgrep summarize DOCUMENT_ID:NODE_ID --root ./documents \
  --model gpt-5.6-luna --reasoning-effort max --service-tier fast --json
```

デフォルトでは回答 worker の起動前に、ホストは必須の Jev ハイブリッド検索を実行し、上限付きの根拠を
推論モデルに渡します。`--jev-model` は Decisions モデルを選び、Codex の `--model` とは
独立した設定です。`--document` で質問の対象を絞り、要約では選択したノードの文書を対象にします。
後続の検索もデフォルトは hybrid です。ツリーの読み取りと明示的な厳密検索は、初期結果を
補う操作として利用できます。報告には、Codex の使用量と併せて、Jev の処理範囲、実際のモデル、
使用量も保持します。

ホストは、実行環境を空にした一時的な stdio app-server スレッドを作成し、上限を設けた
GPTgrep の catalog/tree/search/read ツールを提供します。実際に適用されたサンドボックスと
承認設定を検証し、時間とツール呼び出し回数を制限し、想定外のサーバー要求を拒否し、自身が
起動した子プロセスを終了して回収します。モデルが記述した引用は、その実行中に実際に提供された
根拠を参照し、かつ現在のソースと一致していなければなりません。レポートでは、回答、根拠、
ツール実行記録、実際のモデル／推論強度、使用量を分けて報告します。
ホストは `.gptgrep/host-attempts/` に、非公開かつサイズ制限のあるメタデータ台帳も書き込みます。
後続の段階が失敗または中断しても、完了した Jev 呼び出しを確認できるようにするためです。
ソース文書は変更せず、Codex 子プロセスには Jev の認証情報を渡しません。
引用の同一性の検査だけで、
根拠が回答内容を意味的に裏付けることまで独立に証明できるわけではありません。


`ask` 専用の `--experimental-query-plan` は、ツールを持たない独立した
`gpt-5.6-luna` / `max` / `fast` の計画 worker を先に実行します。元の質問を保持し、
検索表現を最大2件追加します。ルーティングは最大2件を並行実行し、同じソース範囲を
重複除去して既存の候補数上限内にまとめます。その後、Jev が元の質問に対して再評価し、
回答 worker を開始します。このオプションは初期状態で無効で、全段階が呼び出し元の
期限を共有します。計画や分岐の失敗は明示されます。追加候補が元の候補を押し出したり、
待ち時間を増やしたりする場合があり、品質向上は保証しません。`model_attempts` と
`model_usage` は計画呼び出しを個別に記録し、従来の `usage` は回答 worker のみを表します。

```sh
gptgrep ask 'How are offline exports recovered?' ./documents \
  --experimental-query-plan --json
```

`host-complete --input FILE_OR_DASH` は、同じ分離されたローカルモデルを、型付きワークフローの
構成要素として提供します。入力は `{instructions, state, schema}` で、出力にはスキーマ検証済みの
`value`、モデル／推論強度、使用量、入力ダイジェストが含まれます。入力の合計上限はデフォルトで
256 KiB であり、明示的に最大 1 MiB まで変更できます。プロンプトを通知なしに切り詰めることは
ありません。この補完モードに引用検証の保証はありません。比較評価で元の PageIndex の要約・
最適化プロンプトを実行するために定義された、プロバイダーブリッジに対応しています。

app-server API は実験段階です。対応範囲と設定については、
[ホストクレートのドキュメント](crates/gptgrep-host/README.md)に記録されています。

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

Python の検証には 3.12 以上を使い、下記のローカル仮想環境へ固定バージョンの依存関係をインストールします。

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 -m venv .local/eval-venv
.local/eval-venv/bin/python -m pip install --only-binary=:all: -r evals/requirements.lock
.local/eval-venv/bin/python -B -m unittest discover -s evals -p 'test_*.py'
.local/eval-venv/bin/python -B scripts/eval.py --binary target/release/gptgrep --output /tmp/gptgrep-eval.json
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
- OCR や、あらゆる Office／画像形式への対応は主張しません。抽出に必要な機能がなければ、明示的に失敗します。
- 意味検索の候補数には上限があり、関連する文書やノードを取りこぼす可能性があります。Jev は前段で
  除外された根拠を取り戻せず、そのスコアも真実性の保証にはなりません。
- 文書集合の鮮度はスナップショットの契約に基づきます。新しいファイルを追加したら、インデックスを
  再構築してください。検索はその取得に関係するソースだけを再検査し、`status` は既存のインデックス済み
  文書をすべて検査します。
- 現在のインデックス化は世代全体を再構築します。解析結果の差分再利用と、幅広い性能・品質ベンチマークは
  今後の課題です。

独自に開発したコードは [MIT ライセンス](LICENSE)で提供します。依存ライブラリとネイティブ
ランタイムの通知文書については、[THIRD_PARTY.md](THIRD_PARTY.md)を参照してください。

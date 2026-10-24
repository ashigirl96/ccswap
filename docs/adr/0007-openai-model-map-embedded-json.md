# OpenAI モードのモデルの対応表は root の JSON をビルド時に埋め込む

Claude Code のモデルの alias から OpenAI のモデルへの対応表は、repo の root の `openai-models.json` に置き、`include_str!` でビルド時に binary に埋め込む。実行時の上書きは許さない。JSON にしたのは、既存の `serde_json` で読めて依存が増えないため。上書きを許さないのは、一度入れると外しにくく、今は必要が無いため。

workspace の外の path を埋め込むので `cargo package` は通らない。ccswap は crates.io に publish せず、`cargo install --git` と .app で配るので困らない。

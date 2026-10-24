# OpenAI モードの変換 proxy は ccswap が持ち、menubar で動かす

## 変換は自前で書く

Claude Code の Anthropic Messages の要求を OpenAI の Responses に変換する proxy は、ccswap が自分で持つ。新しい依存は入れず、std の TCP と既存の `ureq` で書き、Claude Code が実際に送るもの（text、tool use、SSE の逐次中継）だけを変換する。thinking は捨てる。

外部の proxy（LiteLLM、claude-code-router など）は、Sign in with ChatGPT の token を受け取る口が無い。Sign in with ChatGPT の制約（system を `instructions` に移す、`max_output_tokens` などを送らない、`stream:true` 必須）にも合わせていない。変換用の Rust の crate（`anyllm_translate` など）は利用実績が小さく、制約に合わせる部分は結局自分で書く。そのため、変換の芯を外に預ける利点が薄い。

## proxy は menubar のプロセスで動かす

proxy は、LaunchAgent で常駐する menubar のプロセスの中で、`127.0.0.1` の固定のポートで待ち受ける。session ごとに `ccswap run` の中で立てる案もあったが、menubar と同じライフサイクルにして、常駐物を増やさないほうを選んだ。menubar が動いていなければ OpenAI モードは使えず、`ccswap run` はそう伝えて止まる。

起動中の session は起動時の `ANTHROPIC_BASE_URL` と `ANTHROPIC_AUTH_TOKEN` を持ち続けるので、ポートは固定にし、token も一度作ったら保存して使い回す。menubar が再起動しても、起動中の session がつながり続けるようにするため。

## Claude Code は `~/.claude/settings.json` の `env` で OpenAI に向ける

推論先を OpenAI にすると、ccswap は `~/.claude/settings.json` の `env` に `ANTHROPIC_BASE_URL`、`ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_DEFAULT_*_MODEL` を書く。Anthropic に戻すと、それらを消す。`claude` を直接起動しても OpenAI に向くようにするため。ccswap が書き換えるユーザーの設定はこのファイルだけで、触るのは `env` のうち上の key だけにする。

`ccswap run` が起動時に env を足す案もあった。ユーザーの設定に触れずに済むが、普段 `claude` を直接起動する使い方では OpenAI モードが効かないため採らなかった。

`ANTHROPIC_BASE_URL` を常に proxy に向け、推論先が Anthropic のときは素通しする案もあった。settings.json を一度書くだけで済み、切り替えも起動中の session に要求ごとに効く。だが、`ANTHROPIC_BASE_URL` が `api.anthropic.com` 以外を指す間、Claude Code は Remote Control と音声入力を無効にする。menubar が止まっている間は、Anthropic への推論も止まる。この 2 つの影響を OpenAI モードの間だけに閉じるため、採らなかった。

推論先は ccswap の側に保存しない。`~/.claude/settings.json` の `env` の `ANTHROPIC_BASE_URL` が ccswap の proxy を指していれば OpenAI、それ以外は Anthropic と読む。同じ状態を 2 か所に持つと、ユーザーが settings.json を手で直したときにずれるため。

`ANTHROPIC_AUTH_TOKEN` は必ず設定する。設定しないと、Claude Code は claude.ai の OAuth の token を proxy に送る。

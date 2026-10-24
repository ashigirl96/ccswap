# OpenAI モードは ccswap 自身が Sign in with ChatGPT でログインする

## Codex のアカウントの token を推論に使わない

OpenAI モードの推論には、ccswap が自分の OAuth client（Sign in with ChatGPT の動的登録）でログインして得た token を使い、`api.openai.com/v1/responses` に送る。Codex のアカウントの `auth.json` にある token は使わない。理由は 2 つある。

- Codex の token は Codex CLI の client に発行されたもので、送り先は Codex 専用の `chatgpt.com/backend-api` になる。Codex CLI 以外がそれを名乗って使う経路は規約上グレーで、Sign in with ChatGPT の文書は `backend-api` に送らないよう求めている。
- ADR 0004 のとおり、ccswap は Codex の token を自分で refresh しない。推論のために token を回すと、Codex 側のコピーが `refresh_token_reused` で失効する。

Sign in with ChatGPT の利用は Codex と同じプランの枠から引かれるので、Codex のアカウントと同じ枠を、別の token で使うことになる。

`OPENAI_API_KEY`（従量課金）も検討した。手間は最も少ないが、サブスクの枠を Claude Code から使うという目的に合わないため採らなかった。

## 推論用ログインは Codex のアカウントごとにファイルに置く

推論用ログインは Codex のアカウントごとに持ち、`~/.claude-swap-backup/openai/` の下のファイル（dir 0700、file 0600）に置く。Keychain には入れない。access token は 1 時間で切れ、refresh のたびに refresh token も入れ替わる。そのため、複数の要求が同時に refresh しても失効しないよう、store の `.lock` を取ってから「読む → refresh → 書く」を 1 回で行う。既存の `.lock` とファイルの atomic な書き込みで守れるので、1 時間ごとに `security` で Keychain を書き換えるより単純になる。

ccswap が token を自分で refresh するのはこの資格情報だけで、ADR 0004 の「Codex の token は回さない」は変わらない。

## 推論には active の Codex のアカウントの推論用ログインを使う

proxy は要求のたびに live の Codex のアカウントを見て、その推論用ログインを使う。OpenAI モード専用の active は持たない。使っている ChatGPT のアカウントを 1 つに保ち、切り替えを既存の Codex の switch に任せるため。そのため `ccswap codex switch` は、起動中の Claude Code の session の推論先のアカウントも次の要求から変える。active のアカウントに推論用ログインが無ければ、他のアカウントに回さずにエラーを返す。

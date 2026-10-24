# ccswap

ccswap は、AI のコーディング CLI に登録した複数のアカウントを切り替え、それぞれの使用量を表示する。

## Language

**provider**:
ccswap がアカウントを管理する CLI の種類。Claude Code と Codex の 2 つを指す。
_Avoid_: backend、tool、vendor

**推論先**:
Claude Code が推論の要求を送る先。Anthropic（既定）と OpenAI のどちらか。provider とは別の軸で、Claude Code という provider の推論先が OpenAI になることがある。
_Avoid_: provider、upstream、backend、route

**OpenAI モード**:
Claude Code の推論先が OpenAI である状態。
_Avoid_: OpenAI provider

**推論用ログイン**:
OpenAI モードの推論に使うために、ccswap が Codex のアカウントごとに持つ ChatGPT へのログイン。Codex CLI が使う Codex のアカウントのログインとは別物で、利用の枠は同じプランから引かれる。
_Avoid_: OpenAI ログイン、SIWC ログイン

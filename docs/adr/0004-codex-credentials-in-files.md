# Codex の credential は keychain ではなくファイルに置く

## managed home の `auth.json` にバイト列のまま置く

Codex のアカウントの credential は、`~/.claude-swap-backup/codex/homes/<番号>/auth.json` に、Codex が書いたバイト列のまま置く（dir 0700、file 0600）。Claude のアカウントのように keychain の item には入れない。理由は 3 つある。

- Codex の既定の `cli_auth_credentials_store = "file"` では、Codex は live の `<CODEX_HOME>/auth.json` をそのまま読む。切り替えはファイルの差し替えで済み、keychain を経由する理由がない。
- ccswap は Codex の token を自分で refresh しない。refresh token は回転するので、ccswap が叩くと Codex 側のコピーが `refresh_token_reused` で失効する。期限切れのアカウントは、その managed home を `CODEX_HOME` にした `codex app-server` に refresh させる。そのためには、Codex が読み書きできる `CODEX_HOME` のディレクトリが要る。
- `codex login` も `CODEX_HOME` に `auth.json` を書く。新しいアカウントは、managed home を作ってその中で `codex login` を走らせれば登録できる。

中身は組み立て直さず、バイト列ごとコピーする。Codex が足したフィールドを ccswap が落とさないため。

## store は `codex/` に分け、Claude の store には触れない

台帳は `codex/accounts.json` に置き、番号は Claude の slot とは別の番号空間にする。upstream の claude-swap は `codex/` を知らないので、upstream と共有する `sequence.json` や keychain の item との互換は壊れない。

台帳の read-modify-write は、Claude 側と同じ store の `.lock` を取ってから行う（ADR 0001 の方針と同じ）。CLI、menubar、widget が同時に書いても登録が消えないため。

## keyring の構成には対応しない

`cli_auth_credentials_store` が `keyring` / `auto` / `ephemeral` のとき、Codex は `auth.json` を使わないか、消す。この構成では切り替えは何も書かずにエラーで終える。ファイルを差し替えても Codex は読まず、keychain の Codex の item を ccswap が書き換えると、壊したときに戻せないため。

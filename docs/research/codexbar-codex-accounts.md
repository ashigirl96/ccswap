# CodexBar の Codex（ChatGPT サブスクリプション）アカウント管理

調査日: 2026-10-23。一次資料は次の 2 リポジトリのソースとリポジトリ内 docs に限った。ビルドや実行はしていない。

| 略記 | リポジトリ | commit |
|------|-----------|--------|
| `CB:` | https://github.com/steipete/CodexBar | `6ad1ba02d3ca81e6346fcf2a7302f6af6b2b3b44` |
| `CX:` | https://github.com/openai/codex | `d6817454d76885e9d0e118bbf82e507aed6f715a` |

`CB:path:L10-L20` は `https://github.com/steipete/CodexBar/blob/6ad1ba02d3ca81e6346fcf2a7302f6af6b2b3b44/path#L10-L20` を指す。`CX:` も同様に `https://github.com/openai/codex/blob/d6817454d76885e9d0e118bbf82e507aed6f715a/` を前に付けて読む。ソースに書かれていない事柄は「推測」と明記した。

## 結論

| # | 問い | 答え |
|---|------|------|
| 1 | アカウントの追加 | `~/Library/Application Support/CodexBar/managed-codex-homes/<UUID>/` を作り、`CODEX_HOME` をそこに向けて `codex login` を子プロセスで実行する。ログイン後にその home の `auth.json` から email と account_id を読み、メタデータを登録する。CodexBar 自身は OAuth フローを実装していない。今の `~/.codex/auth.json` の取り込みは、切り替え時に退避先の managed home を作る形で行う |
| 2 | 保存と切り替え | アカウントごとに独立した `CODEX_HOME` ディレクトリを持ち、中の `auth.json` をそのまま保存する。台帳は `managed-codex-accounts.json`（0600、flock で直列化）。Keychain は使わない。切り替え（promote）は、今の `~/.codex/auth.json` のバイト列を managed home に退避してから、選んだアカウントの `auth.json` のバイト列で live の `auth.json` を atomic rename で置き換える。そのあと、動いている `codex app-server daemon` だけを再起動する |
| 3 | 使用量の取得 | 優先順は PAT → OAuth（`GET https://chatgpt.com/backend-api/wham/usage`、`Authorization: Bearer <access_token>` と `ChatGPT-Account-Id`）→ CLI RPC（`codex -s read-only -a never app-server` に `account/read` と `account/rateLimits/read`）。Web dashboard のスクレイプは明示指定したときだけ使う。session log（`~/.codex/sessions/**/*.jsonl`）はコスト推定に使い、rate limit の取得には使っていない |
| 4 | token refresh | CodexBar は refresh しない。`auth.json` の所有者は Codex CLI だという設計で、期限切れ（access_token の JWT `exp` の 5 分前、`exp` が無ければ `last_refresh` から 8 日）を検出すると、`codex app-server` を起動して CLI に refresh させる。`CodexTokenRefresher`（`https://auth.openai.com/oauth/token`、client_id `app_EMoamEEZ73f0CkXaXp7hrann`）は残っているが、本番経路からは呼ばれていない |
| 5 | 同一性の判定 | 主キーは `tokens.account_id`（無ければ id_token の `https://api.openai.com/auth.chatgpt_account_id`）を小文字化したもの。email（id_token の `email` か `https://api.openai.com/profile.email`）は補助キーで、両方に email があれば一致も求める。台帳の重複排除は `email + workspaceAccountID` の組で行う。JWT の署名は検証しない |

openai/codex 側:

- `auth.json` のスキーマは `auth_mode?`, `OPENAI_API_KEY`, `tokens{id_token, access_token, refresh_token, account_id, oauth_provenance?, login_owner_id?}`, `last_refresh`, ほかに `agent_identity` / `personal_access_token` / `bedrock_*`。
- `cli_auth_credentials_store` は `file`（既定）/ `keyring` / `auto` / `ephemeral`。macOS の keyring は service が `"Codex Auth"`、account が `"cli|" + sha256(canonical CODEX_HOME) の先頭 16 hex`。keyring に保存すると `auth.json` は削除される。
- 実行中の codex は auth をメモリにキャッシュし、`auth.json` を監視していない。再読込するのは、proactive refresh の直前、401 を受けたとき、login や logout の後に限られる。そのとき読み直した account_id がキャッシュと違えば、reload も refresh もせずに「別アカウントに切り替わった」というエラーで止まる。

## 詳細

### 1. アカウントの追加（ログイン・取り込み）

#### 1.1 新規ログイン: 専用 `CODEX_HOME` で `codex login` を回す

`ManagedCodexAccountService.authenticateManagedAccount` の流れ（`CB:Sources/CodexBar/ManagedCodexAccountService.swift:L221-L318`）:

1. `ManagedCodexHomeFactory.makeHomeURL()` で `~/Library/Application Support/CodexBar/managed-codex-homes/<UUID>/` を作る（`L43-L76`）。
2. `CodexLoginRunner.run(homePath:timeout:120)` を呼ぶ。環境変数 `CODEX_HOME` を差し替え（`CB:Sources/CodexBarCore/CodexHomeScope.swift:L54-L59`）、`/usr/bin/env <codex> login` を実行する（`CB:Sources/CodexBar/CodexLoginRunner.swift:L4-L25`、`CB:Sources/CodexBar/CLILoginRunner.swift:L32-L33`）。ブラウザでの承認は Codex CLI に任せる。
3. 成功したら、その home の `auth.json` から id_token の claims を読み、email と account_id を得る（`L235-L246`。読み出しは `CB:Sources/CodexBarCore/UsageFetcher.swift:L1260-L1280`）。email が無ければ失敗にする。
4. 同じ email で複数の workspace に入っていれば、`GET https://chatgpt.com/backend-api/accounts` で一覧を取り（`CB:Sources/CodexBarCore/Providers/Codex/CodexOpenAIWorkspaceResolver.swift:L37`, `L71-L80`）、NSAlert で選ばせる（`ManagedCodexAccountService.swift:L139-L180`, `L352-L369`）。
5. 台帳のロックを取り、既存レコードと照合（後述の同一性判定）してから upsert する。同じアカウントの再認証なら古い managed home を消す（`L261-L316`）。

`codex login` は `CODEX_HOME` が存在するディレクトリであることを要求する（`CX:codex-rs/utils/home-dir/src/lib.rs:L13-L40`）。CodexBar がログイン前にディレクトリを作るのはこのため。

推測: 新しく作った managed home には `config.toml` が無いので、`cli_auth_credentials_store` は既定の `file` になり、`auth.json` がファイルとして作られる（既定値は `CX:codex-rs/config/src/types.rs:L134-L146`、`CX:codex-rs/config/defaults.toml:L7`）。CodexBar 側に keyring モードを扱うコードは無く、docs も「CLI の keyring の credentials は読まない」と書いている（`CB:docs/codex.md:L370-L373`）。

#### 1.2 既存の `~/.codex/auth.json` の取り込み

専用の import 操作は無い。代わりに次の 2 つで取り込まれる。

- **System アカウント**: `~/.codex`（または `$CODEX_HOME`）の `auth.json` を、常に「System」行として読む（`CodexActiveSource.liveSystem`、`CB:Sources/CodexBarCore/Config/CodexActiveSource.swift:L3-L6`）。
- **切り替え時の退避（displaced live preservation）**: 別アカウントを promote すると、live の `auth.json` のバイト列を新しい managed home にコピーし、managed account として台帳に追加する（`CB:Sources/CodexBarCore/Providers/Codex/CodexAccountPromotionExecution.swift:L80-L123`）。既に同じ identity の managed account があれば、その home の `auth.json` を live の最新バイト列で上書きする（`L203-L279`）。

このほか、上級者向けに `~/.codexbar/config.json` の `providers[].codexProfileHomePaths` で既存の Codex home を登録できる。CodexBar はその home を読むだけで、コピーも再認証も削除もしない（`CB:docs/codex.md:L206-L227`）。

### 2. 複数アカウントの保存と切り替え

#### 2.1 保存形式

| 物 | 場所 | 中身 |
|----|------|------|
| 台帳 | `~/Library/Application Support/CodexBar/managed-codex-accounts.json`（`CB:Sources/CodexBarCore/ManagedCodexAccountStore.swift:L173-L184`） | `{version: 3, accounts: [...]}`（`L28`）。各要素は `id`(UUID), `email`(小文字), `providerAccountID`, `workspaceLabel`, `workspaceAccountID`, `authFingerprint`(auth.json の SHA-256), `managedHomePath`, `createdAt`, `updatedAt`, `lastAuthenticatedAt`（`CB:Sources/CodexBarCore/CodexManagedAccounts.swift:L4-L14`, `L87-L111`） |
| credentials | `managed-codex-homes/<UUID>/auth.json` | Codex CLI が書いた `auth.json` をそのまま置く。CodexBar は中身を組み立て直さず、バイト列ごとコピーする |
| ロック | `managed-codex-accounts.json.lock` | `flock(LOCK_EX|LOCK_NB)`。取れなければ即座に `busy` を返す（`CB:Sources/CodexBarCore/ManagedCodexAccountLock.swift:L23-L56`）。CodexBar のプロセス同士でしか効かず、Codex CLI はこのロックを取らない（`CB:docs/codex.md:L145-L147`） |

書き込みはすべて `CredentialFileWriter.writePrivate` を通す。同じディレクトリに 0700 の staging ディレクトリを作り、0600 + `O_EXCL` で書いて fsync し、`rename(2)` で置き換える（`CB:Sources/CodexBarCore/CredentialFileWriter.swift:L20-L79`）。

Keychain は Codex の credentials には使っていない。CodexBar が Keychain に置くのは、OpenAI web dashboard 用の browser cookie のキャッシュ（service `com.steipete.codexbar.cache`, account `cookie.codex`）だけ（`CB:docs/codex.md:L259`）。

#### 2.2 2 種類の「active」

- **CodexBar が表示に使うアカウント**（`codexActiveSource`: `liveSystem` / `managedAccount(id)` / `profileHome(path)`）。これを変えても `~/.codex` には触れない。使用量を取るときは、子プロセスや HTTP の環境で `CODEX_HOME` をそのアカウントの home に向けるだけ（`CB:Sources/CodexBar/ProviderRegistry.swift:L184-L196`）。
- **System の Codex アカウント**、つまり `~/.codex/auth.json`（`CODEX_HOME` があればそちら）。これを書き換えるのが「promote」。

#### 2.3 promote（live の auth.json の書き換え）

`CodexAccountPromotionTransaction.promoteLocked`（`CB:Sources/CodexBarCore/Providers/Codex/CodexAccountPromotionTransaction.swift:L139-L206`）の手順:

1. 台帳のロックを取る（`L124-L128`）。
2. live home が managed home と同じパスなら中止する。上書きすると退避したものが消えるため（`L148-L155`）。
3. 既に同じ identity なら何もしない（`convergedNoOp`、`L157-L164`, `L208-L245`）。
4. managed 側で選んだ workspace が、その `auth.json` の既定の workspace と違うなら中止する（`L166-L168`）。
5. live の `auth.json` を managed home に退避する（新規 import か、既存の上書き。1.2 を参照）。
6. target と live の `auth.json` を読み直し、事前に読んだバイト列と一致しなければ中止する（`L184-L189`）。退避先も読み直して照合する（`L190-L192`, `L247-L266`）。
7. target の `auth.json` のバイト列で live の `auth.json` を atomic に置き換える（`DefaultCodexLiveAuthSwapper`, `L41-L53`, `L193-L197`）。

その後 `CodexAccountPromotionService` が次を行う（`CB:Sources/CodexBar/CodexAccountPromotionService.swift:L87-L98`）。

- 表示用の active を `liveSystem` にする。
- `<home>/app-server-daemon/{daemon.pid,app-server.pid}` に記録されたプロセスが `codex app-server --listen unix://` なら、`codex app-server daemon version` で socket のパスを確かめてから `codex app-server daemon restart` を実行する。失敗したら「手動で再起動して」という注記を出す（`CB:Sources/CodexBar/CodexAppServerDaemon.swift:L16-L46`）。
- 普通の対話型 `codex` プロセスは再起動しない（`CB:docs/codex.md:L164-L167` に「running Codex processes alone」とある）。

CLI からも `codexbar codex-accounts promote <uuid|email>` で同じ処理を呼べる（`CB:docs/codex.md:L140-L170`）。

### 3. 使用量（5h / weekly / credits）の取得

#### 3.1 手段と優先順位

`CodexProviderDescriptor.resolveStrategies`（`CB:Sources/CodexBarCore/Providers/Codex/CodexProviderDescriptor.swift:L150-L172`）:

| source mode | 試す順 |
|-------------|--------|
| auto（既定） | PAT → OAuth → CLI RPC。ただし managed workspace を選んでいるときは CLI を外す（CLI に `ChatGPT-Account-Id` を渡せないため） |
| oauth | OAuth → native refresh 用の CLI（credentials が期限切れのときだけ有効、`L341-L376`） |
| cli | CLI RPC |
| web | OpenAI web dashboard |
| api | PAT |

auto で次の手段に移るのは、401、credentials の不在・読めない・tokens が無い・期限切れ、refresh 系のエラーのときに限る。ネットワーク、5xx、decode の失敗では移らず、そのエラーを表示する（`L489-L526`）。

#### 3.2 OAuth（主経路）

- URL: `chatgpt_base_url`（`$CODEX_HOME/config.toml` から読む。既定は `https://chatgpt.com/backend-api/`）+ `/wham/usage`。base に `/backend-api` が含まれなければ `/api/codex/usage` を使う（`CB:Sources/CodexBarCore/Providers/Codex/CodexOAuth/CodexOAuthUsageFetcher.swift:L379-L385`, `L564-L684`）。Codex 本体も同じ規則で選ぶ（`CX:codex-rs/backend-client/src/client.rs:L141-L156`、`CX:codex-rs/backend-client/src/client/rate_limit_resets.rs:L124-L129`）。
- ヘッダ: `Authorization: Bearer <tokens.access_token>`、`ChatGPT-Account-Id: <tokens.account_id>`（managed workspace を選んでいればその ID）、`Accept: application/json`、`User-Agent: CodexBar`（`L399-L424`、上書きは `CodexProviderDescriptor.swift:L421-L433`）。Codex 本体の既定の UA は `codex-cli`（`CX:codex-rs/backend-client/src/client.rs:L295-L305`）。
- レスポンスのうち CodexBar が読むフィールド（`CodexOAuthUsageFetcher.swift:L6-L31`, `L161-L222`, `L227-L236`, `L308-L334`）:

```json
{
  "account_id": "...",
  "plan_type": "pro",
  "rate_limit": {
    "primary_window":   { "used_percent": 15, "reset_at": 1735401600, "limit_window_seconds": 18000 },
    "secondary_window": { "used_percent": 5,  "reset_at": 1735920000, "limit_window_seconds": 604800 },
    "individual_limit": { "...": "spend control" }
  },
  "additional_rate_limits": [ { "limit_name": "...", "metered_feature": "...", "rate_limit": { "...": "..." } } ],
  "credits": { "has_credits": true, "unlimited": false, "balance": 150.0 },
  "spend_control": { "individual_limit": { "...": "..." } }
}
```

  `balance` は数値でも文字列でもよい。枠の分類は `limit_window_seconds/60` で決め、300 分なら session、10080 分なら weekly、43200 分なら monthly とする（`CodexProviderDescriptor.swift:L238-L247`）。

- 付随して叩く API:
  - `GET .../wham/rate-limit-reset-credits`。ヘッダに `OpenAI-Beta: codex-1` と `originator: Codex Desktop` を付ける（`CodexOAuthUsageFetcher.swift:L524-L541`）。
  - `.../accounts/<id>/spend-controls/current-user/monthly-usage`
  - workspace の `.../remaining_balance`
- 使用量の取得と reset-credit の取得は、同じ credential のスナップショットで行う。途中で `auth.json` を読み直すと競合を招くため（`CB:docs/codex-oauth.md` の Step 4 末尾）。

#### 3.3 CLI RPC（`codex app-server`）

- 起動するコマンドは `codex -s read-only -a never app-server`。stdin/stdout に JSON-RPC を流し、`initialize`（`clientInfo`）→ `initialized` → `account/read` → `account/rateLimits/read` の順に呼ぶ（`CB:Sources/CodexBarCore/UsageFetcher.swift:L959-L960`, `L1013-L1025`）。
- `account/read` の応答は `{account: {type: "chatgpt", email, planType} | {type: "apiKey"}, requiresOpenaiAuth}`。`account/rateLimits/read` の応答は `{rateLimits: {limitId, limitName, primary{usedPercent, windowDurationMins, resetsAt}, secondary, credits{hasCredits, unlimited, balance(文字列)}, individualLimit, planType, rateLimitReachedType}, rateLimitsByLimitId}`（`L747-L877`）。
- Codex 側では、この RPC が backend の `wham/usage` を呼んでいる（`CX:codex-rs/app-server/src/request_processors/account_processor.rs:L1152-L1181`、`CX:codex-rs/backend-client/src/client/rate_limit_resets.rs:L23-L79`）。
- 子プロセスが期限切れの token を自分で refresh して `auth.json` に書き戻すので、CLI RPC は CodexBar にとって「refresh を CLI に任せる」経路も兼ねている（4 章）。

#### 3.4 その他の手段

- **OpenAI web dashboard**（既定 off）: 画面外の `WKWebView` で `https://chatgpt.com/codex/cloud/settings/analytics#usage` を開き、本文のテキストから 5h / weekly / credits を拾う。cookie は Safari、Chrome、Firefox から取り込み、email でアカウントを照合する（`CB:docs/codex.md:L241-L275`）。
- **PTY の `/status`**: 診断用だけで、自動の更新では使わない（`CB:docs/codex.md:L324-L331`）。
- **session log**: `~/.codex/sessions/YYYY/MM/DD/*.jsonl` はトークンのコスト推定にだけ使う（`CB:docs/codex.md:L351-L401`）。Codex の rollout の `token_count` イベントにも `rate_limits` が入っている（`CX:codex-rs/protocol/src/protocol.rs:L2492-L2550`）が、CodexBar のソースには、ここから rate limit を読むコードが見当たらなかった。
- 推論の応答ヘッダ `x-codex-primary-used-percent` などは Codex 本体が読んでいる（`CX:codex-rs/codex-api/src/rate_limits.rs:L182-L313`）。CodexBar は使っていない。

### 4. token refresh

#### 4.1 CodexBar の方針: refresh しない

- 期限切れの判定（`CB:Sources/CodexBarCore/Providers/Codex/CodexOAuth/CodexOAuthCredentials.swift:L51-L62`）は、access_token の JWT `exp` が今から 5 分以内なら期限切れとする（native の場合）。`exp` が取れなければ `last_refresh` から 8 日を超えたかで判定する。どちらも Codex 本体の判定（`CX:codex-rs/login/src/auth/manager.rs:L214-L215`, `L3313-L3329`）に合わせてある。
- 期限切れなら `nativeRefreshRequired` を投げる。refresh token を使う処理（redeem）は usage の経路から一切呼ばない（`CodexProviderDescriptor.swift:L465-L475`）。docs にある理由は「共有の refresh token を消費して、回転後の token を所有者に渡さなければ、所有者が古い token のまま取り残される」（`CB:docs/codex-oauth.md`「Token Freshness and Ownership」）。
- 所有者が書き換えている途中を読んだ可能性を考え、50ms 間隔で最大 2 回読み直す（`CodexProviderDescriptor.swift:L391-L414`）。Codex の `FileAuthStorage::save` は `truncate + write` で、atomic rename をしない（`CX:codex-rs/login/src/auth/storage.rs:L227-L244`）。そのため読み手は、途中まで書かれたファイルを読みうる。
- 回復は CLI に任せる。oauth モードでは `CodexOAuthNativeRefreshCLIStrategy` が `codex app-server` を起動し、Codex の AuthManager に refresh と書き戻しをさせる（`L341-L376`）。auto モードでは CLI 戦略に移る。
- `CodexTokenRefresher`（`POST https://auth.openai.com/oauth/token`、JSON の body が `{client_id: "app_EMoamEEZ73f0CkXaXp7hrann", grant_type: "refresh_token", refresh_token, scope: "openid profile email"}`、`CB:Sources/CodexBarCore/Providers/Codex/CodexOAuth/CodexTokenRefresher.swift:L7-L8`, `L45-L56`）は残っているが、`Sources/` 内に呼び出し元は無い（grep で確認）。`CodexOAuthCredentialsStore.save`（`CodexOAuthCredentials.swift:L324-L364`）も同じで、本番経路からは使われていない。
- 実行中の codex CLI と競合しないのは、CodexBar が live の `auth.json` に書くのが promote のときだけで、それ以外は読むだけだから。promote では書き換え直前に live のバイト列を照合するが、それでも最後に読んでから rename するまでの間に外部の書き手が割り込む余地は残る、と docs が認めている（`CB:docs/codex.md:L151-L152`）。

#### 4.2 Codex 本体の refresh

- エンドポイントは `https://auth.openai.com/oauth/token`。issuer の provenance があれば `<issuer>/oauth/token` を使い、環境変数 `CODEX_REFRESH_TOKEN_URL_OVERRIDE` で上書きできる（`CX:codex-rs/login/src/auth/manager.rs:L223-L227`, `L1793-L1798`）。client_id は `app_EMoamEEZ73f0CkXaXp7hrann` で、`CODEX_APP_SERVER_LOGIN_CLIENT_ID` で上書きできる（`L1784-L1791`）。
- 書き戻し（`persist_tokens`、`L1647-L1691`）は、プロセス内の mutex を取って storage から読み直す。保存されている tokens が「同じログイン」（`same_login_for_persistence`）でなければ `Account credentials changed during refresh.` で中止する。同じなら id/access/refresh token を差し替え、`last_refresh = now` にして保存する。
- エラーコードは `refresh_token_expired` / `refresh_token_reused` / `refresh_token_invalidated` を見分ける（`L1751-L1778`）。`reused` を区別していることから、refresh token は使うたびに回転する（rotation）と推測する。

### 5. アカウントの同一性判定

- identity の型（`CB:Sources/CodexBarCore/Providers/Codex/CodexIdentity.swift:L3-L35`）は `providerAccount(id)` / `emailOnly(email)` / `unresolved` の 3 つ。account_id があれば必ず `providerAccount` になる。
- account_id の出所は、優先順に `tokens.account_id` → id_token の `["https://api.openai.com/auth"].chatgpt_account_id` → id_token のトップレベルの `chatgpt_account_id`（`CB:Sources/CodexBarCore/UsageFetcher.swift:L1273-L1276`、`CB:Sources/CodexBarCore/Providers/Codex/CodexAccountPromotionPreparation.swift:L218-L245`）。比較の前に trim して小文字化する（`CodexManagedAccounts.swift:L55-L60`）。
- email は id_token の `email`、無ければ `["https://api.openai.com/profile"].email`。plan は `["https://api.openai.com/auth"].chatgpt_plan_type`。
- 照合の規則（`CB:Sources/CodexBarCore/Providers/Codex/CodexAccountReconciliation.swift:L451-L480`）:
  - account_id 同士なら一致を求める。両方に email があれば email の一致も求める。
  - email-only 同士なら email の一致を求める。
  - 種類が違えば不一致とする。
- 台帳の重複排除は `(email, effectiveWorkspaceAccountID)` の組で行う。workspace が無い古いレコードは email だけで扱う（`CodexManagedAccounts.swift:L136-L176`）。同じ email でも workspace（ChatGPT の account_id）が違えば別アカウントになる。
- JWT は base64url の payload を decode するだけで、署名は検証しない（`UsageFetcher.swift:L1492-L1505`、`CodexIdentity.swift:L37-L39` のコメント）。
- Codex 本体の id_token の claims も同じ構造（`CX:codex-rs/login/src/token_data.rs:L86-L114`, `L189-L213`）。

### 6. openai/codex 側の一次情報

#### 6.1 `auth.json` のスキーマ

`AuthDotJson`（`CX:codex-rs/login/src/auth/storage.rs:L47-L73`）と `TokenData`（`CX:codex-rs/login/src/token_data.rs:L10-L40`）:

```jsonc
{
  "auth_mode": "chatgpt",              // 省略可
  "OPENAI_API_KEY": null,              // ChatGPT ログインでも token-exchange で得た key が入りうる
  "tokens": {
    "id_token": "<JWT>",               // 読み込み時に claims へ parse される
    "access_token": "<JWT>",
    "refresh_token": "...",
    "oauth_provenance": { "issuer": "https://auth.openai.com", "client_id": "app_..." }, // 省略可
    "login_owner_id": "...",           // 1 回のログインを識別する。回転しても保たれる。省略可
    "account_id": "..."                // id_token の chatgpt_account_id
  },
  "last_refresh": "2026-10-23T00:00:00Z",
  "agent_identity": null, "personal_access_token": null,
  "bedrock_api_key": null, "bedrock_access_keys": null   // いずれも省略可
}
```

ログインで書かれる値は `CX:codex-rs/login/src/server.rs:L821-L879` にある。`account_id` は id_token の `chatgpt_account_id` から取り、`login_owner_id` には乱数を入れる。`OPENAI_API_KEY` は `obtain_api_key`（token-exchange）の結果で、失敗すれば null になる（`L442-L446`）。

#### 6.2 `cli_auth_credentials_store` と keyring

- 値は `file`（既定）、`keyring`（使えなければ失敗）、`auto`（keyring を優先し、失敗したら file）、`ephemeral`（プロセスのメモリだけ）の 4 つ（`CX:codex-rs/config/src/types.rs:L133-L146`）。packaged client の既定も `"file"`（`CX:codex-rs/config/defaults.toml:L7`）。ローカルの開発ビルドでは keyring と auto を file に読み替える（`CX:codex-rs/core/src/config/mod.rs:L286-L297`）。
- keyring の backend は `Direct`（Windows 以外の既定。OS の keyring に JSON をそのまま入れる）と `Secrets`（暗号化した secrets ファイルの鍵を keyring に置く。feature `SecretAuthStorage` で有効になる）の 2 つ（`CX:codex-rs/config/src/types.rs:L165-L183`、`CX:codex-rs/core/src/config/auth_keyring.rs:L85-L117`）。
- Direct のキー（`CX:codex-rs/login/src/auth/storage.rs:L256-L270`）は、service が `"Codex Auth"`、account が `"cli|" + hex(sha256(canonicalize(CODEX_HOME)))[..16]`。macOS では `keyring` crate の `apple-native` を使う（`CX:codex-rs/keyring-store/Cargo.toml:L19-L21`）。推測: login keychain の generic password になる。
- keyring に保存すると、残っている `auth.json` を削除する（`storage.rs:L328-L345`）。auto で keyring への保存に失敗した場合は file に保存する（`L527-L548`）。

#### 6.3 `codex login` のフロー

- 既定はブラウザの PKCE フロー。issuer は `https://auth.openai.com`、ローカルの redirect は `http://127.0.0.1:1455/auth/callback`（`CX:codex-rs/login/src/server.rs:L77-L78`, `L194`）。authorize の URL は `<issuer>/oauth/authorize` で、scope は `openid profile email offline_access api.connectors.read api.connectors.invoke`。追加パラメータとして `id_token_add_organizations=true`、`codex_cli_simplified_flow=true`、`originator` を付ける（`L575-L607`）。
- コードを token に交換したら、token-exchange で API key を取りにいき、`save_auth` で設定された store に保存する（`L436-L446`, `L821-L879`、`CX:codex-rs/login/src/auth/manager.rs:L1185-L1198`）。
- ほかに `--device-auth`、`--with-api-key`（stdin から読む）、`--with-access-token`、`codex login status` がある（`CX:codex-rs/cli/src/main.rs:L521-L568`, `L1670-L1706`）。

#### 6.4 外部から auth.json を差し替えたときの、実行中の codex の振る舞い

- `AuthManager` は auth をメモリにキャッシュする（`auth_cached`、`CX:codex-rs/login/src/auth/manager.rs:L2439-L2445`）。`auth.json` を監視するコードは見つからなかった（`auth.json` と watch で grep した）。
- storage を読み直すのは次の場面だけ。
  - **proactive refresh の直前**: `auth()` が期限切れを検出すると `refresh_token_with_reason` を呼ぶ。そこではまず `reload_if_account_id_matches(キャッシュの account_id)` を実行する（`L2479-L2492`, `L3126-L3170`）。
    - ディスクの account_id がキャッシュと同じで、tokens が変わっていれば、それを採用して refresh しない（他のプロセスが refresh した場合）。
    - 変わっていなければ自分で refresh する。
    - account_id が違えば何も読み込まず、`REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE`（「その後ログアウトしたか、別のアカウントでサインインしたため refresh できない。サインインし直して」）を Permanent エラーとして返す（`L222`, `L2713-L2745`）。
  - **401 を受けたとき**: `UnauthorizedRecovery` が、まず同じ条件付きの reload を行い、次に refresh する。account_id が違えば同じエラーで終わる（`L1904-L1945`, `L2053-L2091`）。
  - **login / logout の後**、app-server の account 系 RPC の後（`CX:codex-rs/app-server/src/request_processors/account_processor.rs:L478`, `L577`, `L985`, `L1544`）。
- したがって、別アカウントの `auth.json` に差し替えても、実行中の対話型 `codex` は古いアカウントの access token を使い続ける。token が切れるか 401 を受けた時点で account mismatch のエラーになり、新しいアカウントには自動で移らない。CodexBar が `codex app-server daemon` を再起動するのはこのためだと推測する。
- 同じアカウントの新しい token（別のプロセスが refresh したもの）に差し替えた場合は、次の refresh や 401 のときに採用される。

## ccswap（Claude Code のアカウント切り替えツール、Rust 製、macOS keychain に credentials を保存）に Codex 対応を足すときの示唆

1. **アカウントごとに `CODEX_HOME` を分けるのが最も素直**。`codex login` を `CODEX_HOME=<ccswap 管理下の dir>` で実行すれば、ログインと token 管理を Codex に任せられる。ccswap が OAuth を実装する必要はない。ディレクトリは事前に作っておく（Codex は存在しない `CODEX_HOME` を拒否する）。
2. **保存先を keychain にするか**。ccswap の方針に合わせて `auth.json` のバイト列を keychain に入れることもできる。ただし Codex の file モードは `auth.json` そのものを読むので、切り替えるたびにファイルへ書き出す必要がある。CodexBar は Keychain を使わず、0600 のファイルと atomic rename で済ませている。Codex 自身の keyring モード（service `Codex Auth`、account `cli|<sha256(CODEX_HOME)[..16]>`）を ccswap が直接読み書きする手もあるが、キーが `CODEX_HOME` の canonical path に依存すること、`auto` では file に落ちうること、`Secrets` backend では中身が暗号化されたファイル側にあることから、扱いが複雑になる。まずは「ユーザーの `cli_auth_credentials_store` が `file`（既定）のときだけ対応し、keyring や auto なら警告する」のが安全（設計判断）。
3. **切り替えるときは、まず live の `auth.json` を保存元へ書き戻す**。Codex の CLI は live の `auth.json` を refresh して回転させるので、ccswap が持つ古いコピーの refresh token は使えなくなる（`refresh_token_reused`、rotation の推測は 4.2）。CodexBar の「displaced live preservation」と同じく、切り替え直前に live の内容を、同じ identity（account_id と email）の保存先へ必ず書き戻す。ccswap の Claude 側で同じことをしているなら、その仕組みを流用できる。
4. **live の `auth.json` は atomic rename で置き換える**。Codex 自身は truncate + write なので、読み手は書きかけを読みうる。ccswap は staging ファイルから rename し、0600 にする。書き換える直前にもう一度読んで事前のバイト列と比べれば、Codex の refresh との競合を狭められる（完全には消えない）。
5. **ccswap は token を refresh しない**。`auth.openai.com/oauth/token` を ccswap が叩くと、回転後の token を Codex に渡し損ねたときに Codex 側が `reused` で失効する。期限切れのアカウントの使用量を見たいなら、CodexBar のように `CODEX_HOME=<その dir> codex -s read-only -a never app-server` を起動して `account/rateLimits/read` を呼び、refresh と書き戻しを Codex に任せる。
6. **使用量は `GET https://chatgpt.com/backend-api/wham/usage` で取れる**。ヘッダは `Authorization: Bearer <access_token>` と `ChatGPT-Account-Id: <account_id>`。`rate_limit.primary_window` / `secondary_window` の `used_percent`・`reset_at`・`limit_window_seconds` と `credits` が、ccswap のウィジェットの 5h / weekly 表示にそのまま対応する。非公開 API なので、decode に失敗しても落ちない作りにしておく（CodexBar はフィールドを寛容に decode している）。
7. **同一性のキーは `tokens.account_id`（小文字化）+ id_token の email**。同じ email でも workspace（Team / Business）が違えば別アカウントとして扱う。表示名の email は id_token の claims から取れる。署名は検証しなくてよい（どちらの実装も検証していない）。
8. **実行中の codex は切り替えに追従しない**。対話中の `codex` は古いアカウントのまま動き、token が切れた時点で「別アカウントにサインインした」というエラーで止まる。ccswap の UI では「切り替えは新しく起動した codex から有効」と明示する。`~/.codex/app-server-daemon/` に daemon がいれば、`codex app-server daemon restart` を案内するか実行する。
9. **ccswap 自身の書き込みは flock で直列化する**。メニューバー、CLI、ウィジェットが同時に台帳を書かないよう、CodexBar と同じく `.lock` ファイルに `flock(LOCK_EX|LOCK_NB)` をかける。Codex 本体はこのロックを見ないので、Codex との競合は 4 の照合で防ぐ。

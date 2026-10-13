# upstream と意図して変える点 (spec に無いもの)

振る舞いの正は claude-swap v0.26.0 (`upstream/`) で、意図して変える点は spec (ashigirl96/ccswap#1) の「意図して upstream と変える点」に列挙してある。実装の途中で、spec に無い違いをいくつか入れた。ここにまとめて記録する。

## store の `.lock` を取るコマンドを増やす

`add`、`add-token`、`remove`、`disable` / `enable`、`alias`、`import` は、`sequence.json` を読んで書き戻す間だけ store の `.lock` を取る。upstream はこれらで lock を取らない。Python 版と同時に動かしたとき、`switch` などとの read-modify-write の競合で登録が消えるのを防ぐため。profile の取得などの network 呼び出しの間は lock を持たない。

## migration は root の検査の後に走らせる

upstream は root の検査より前に store の migration を走らせる。ccswap は root の検査の後に走らせる。root で実行したときに利用者の store を root の所有で書き換えないため。

## menubar の閾値の変更は動いている engine にそのまま反映する

upstream は auto-switch の閾値を変えると engine を止めて作り直す。ccswap は `LoopHandle::apply_threshold` と `wake` で、動いている engine に反映する。menubar の操作 (switch、disable など) は main thread ではなく worker thread で実行し、エラーは次の 1 秒ごとの同期で alert に出す。AppKit の main thread を network 呼び出しで止めないため。

## 退避コピーに失敗したら `CredentialWriteError` にする

通常の switch で、持ち主の分からない live の credential を退避 (unclaimed) に書けなかったとき、upstream は OSError の traceback を出して終了コード 1 で落ちる。ccswap は `CredentialWriteError` のエラーとして報告し、終了コード 1 で終わる。`--json` でも envelope の形で返すため。

## remove / purge の確認で EOF なら "Cancelled" を出す

`remove` と `purge` の確認の prompt で stdin が EOF のとき、upstream は EOFError で落ちる。ccswap は `Cancelled` を出して何も消さずに終わる。pipe から実行したときに traceback を出さないため。

## 通知用に Info.plist を binary に埋め込む

menubar の通知を出すため、bundle id `io.github.ashigirl96.ccswap` の Info.plist を binary の `__TEXT,__info_plist` に埋め込む。bundle に入っていない binary からは `NSUserNotificationCenter` が使えないため。upstream は rumps が Python の bundle を使うので、これが要らない。

## uuid の back-fill に失敗しても credential は記録する

active の credential の所有者を profile で確かめる処理 (usage の取得時に backup を resync する前の照合) で、slot の `uuid` を back-fill する書き込みに失敗しても、ccswap は所有者を確認できたものとして credential を記録する。upstream はこの失敗を例外として上に投げる。back-fill は次の確認でやり直せる付加情報で、そのために credential の更新まで捨てる理由が無いため。失敗は DEBUG の log に残す。

## `ccswap upgrade` は Releases と `cargo install --git` を案内する

`ccswap upgrade` (`update`) が表示する手順は、GitHub Releases の最新版の URL と `cargo install --git https://github.com/ashigirl96/ccswap ccswap` にする。spec は uv / pipx を呼ばないことだけを決めていて、表示する中身は決めていないため。

## コマンドを打つよう促す文言は `ccswap` と書く

"Re-add with: ccswap --add-account --slot N" のように、利用者に打つコマンドを示す文言は `ccswap` と書く。upstream は `cswap` と書く。binary の名前を `ccswap` に変えたので、示したコマンドがそのまま動くようにするため。log の文言や、コマンドを指していない説明は upstream と同じ `cswap` のままにする。

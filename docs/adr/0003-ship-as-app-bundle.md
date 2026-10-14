# release を ccswap.app の bundle で配る

## Rust の binary を .app の main executable にする

release の tarball には `ccswap.app` を入れ、Rust の `ccswap` を `Contents/MacOS/ccswap`、Swift の WidgetKit extension を `Contents/PlugIns/CcswapWidget.appex` に置く。PATH の `ccswap` は `Contents/MacOS/ccswap` への symlink にする。widget extension は、それを埋め込んだ app の bundle に入れないと system に登録されないため。

widget 専用の小さな host app を別に配る案もあった。だが、その案では「widget の持ち主の app」が menubar とは別の process になる。menubar から widget の再読み込みを頼むときに、その app 自身から呼んだことにならない。

`cargo install` で入れた binary には widget が付かない。CLI と menubar は今までどおり動く。

## 署名は ad-hoc のままにする

bundle 全体を `codesign --sign -` で署名する。Apple Developer の team も notarization も要らない。macOS 26 の実機で、ad-hoc 署名の widget がギャラリーに出て、描画も AppIntent のボタンも動くことを確かめた。

## widget とは App Group ではなく専用 directory で受け渡す

App Group の識別子には team ID の prefix が要り、ad-hoc 署名では使えない。代わりに widget extension の sandbox に `com.apple.security.temporary-exception.files.home-relative-path.read-write` で `/.claude-swap-backup/widget/` だけを許す。menubar がそこに `feed.json` を書き、widget のボタンは `request.json` を書く。

許可を `~/.claude-swap-backup/` 全体ではなく `widget/` に絞るのは、同じ store にある credential に widget の sandbox から触れさせないため。temporary-exception は App Store では通らないが、ccswap は App Store で配らない。

## `ccswap widget --install` は bundle を一度起動する

登録は `lsregister -f`、`pluginkit -a`、`open -g -j ccswap.app --args --version` の順で行う。最後の起動を省くと、`linkd` が bundle の App Intents を索引しない。そのため widget のボタンが「metadata が無い」という error で動かない。

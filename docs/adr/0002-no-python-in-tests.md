# test と CI に Python を使わない

## upstream との差分 test をやめる

black-box test を upstream の Python 版 `cswap` にも流す差分 test (`CCSWAP_TEST_UPSTREAM`、CI の `differential` job、`upstream` submodule) をやめる。test と CI を動かすのに Python と uv を要らなくするため。期待値は、移植した assertion が ccswap 側の値として固定している。upstream の変更を取り込むときは、upstream の source と test を人が読んで assertion に反映する。

## test の fake は Rust で書く

fake の `security`、`claude`、`launchctl` は workspace の `ccswap-test-fakes` crate (`publish = false`) の 1 つの binary にする。harness が test の初回に別の target dir へ build し、各 test の root に symlink で置く。`ccswap` package に入れないので、`cargo install` で fake が配られない。

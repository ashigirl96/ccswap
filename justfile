set shell := ["sh", "-eu", "-c"]

out := "target/app"
app_dir := env("HOME") / "Applications"
bin_dir := env("HOME") / ".local/bin"

# Build the universal ccswap.app the release ships, into target/app.
app:
    #!/bin/sh
    set -eu
    version=$(cargo pkgid -p ccswap | sed 's/.*[#@]//')
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
        cargo build --release --locked -p ccswap --target "$target"
    done
    mkdir -p {{out}}
    lipo -create -output {{out}}/ccswap \
        target/aarch64-apple-darwin/release/ccswap \
        target/x86_64-apple-darwin/release/ccswap
    codesign --force --sign - {{out}}/ccswap
    sh macos/build-app.sh {{out}}/ccswap "$version" {{out}}
    echo "Built {{out}}/ccswap.app ($version)"

# Replace ~/Applications/ccswap.app, point ~/.local/bin/ccswap at it, and restart the menu bar and widget.
install: app
    mkdir -p {{app_dir}} {{bin_dir}}
    rm -rf {{app_dir}}/ccswap.app
    cp -R {{out}}/ccswap.app {{app_dir}}/
    ln -sfn {{app_dir}}/ccswap.app/Contents/MacOS/ccswap {{bin_dir}}/ccswap
    {{bin_dir}}/ccswap widget --install
    {{bin_dir}}/ccswap menubar --install-service
    {{bin_dir}}/ccswap --version

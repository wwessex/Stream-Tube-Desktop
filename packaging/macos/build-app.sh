#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ $(uname -s) != Darwin ]]; then
    echo "Build the app on macOS with Xcode Command Line Tools installed." >&2
    exit 1
fi

safari_prototype=${STREAMTUBE_SAFARI_PROTOTYPE:-0}
safari_test_id=${STREAMTUBE_SAFARI_TEST_ID:-}
if [[ -n $safari_test_id ]]; then
    if [[ ! $safari_test_id =~ ^[a-z0-9]([a-z0-9-]{0,30}[a-z0-9])?$ ]]; then
        echo "STREAMTUBE_SAFARI_TEST_ID must be 1–32 lowercase ASCII letters, digits or hyphens, with a letter or digit at each end." >&2
        exit 2
    fi
    safari_prototype=1
fi

app_name="Stream Tube"
bundle_id=io.github.avaneesh-inamdar.StreamTube
url_scheme=streamtube
if [[ $safari_prototype == 1 ]]; then
    app_name="Stream Tube Safari Prototype"
    bundle_id="$bundle_id.SafariPrototype"
    url_scheme=streamtube-safari-prototype
    if [[ -n $safari_test_id ]]; then
        app_name="Stream Tube Safari Test ($safari_test_id)"
        bundle_id="$bundle_id.Test.$safari_test_id"
        url_scheme="streamtube-safari-test-$safari_test_id"
    fi
fi

cargo build --release --locked >&2
metadata=$(cargo metadata --format-version 1 --no-deps --locked)
version=$(printf '%s' "$metadata" | plutil -extract packages.0.version raw -o - -)
target_dir=$(printf '%s' "$metadata" | plutil -extract target_directory raw -o - -)
binary="$target_dir/release/streamtube"
minimum_system=$(xcrun vtool -show-build "$binary" | awk '$1 == "minos" { print $2 }')
[[ -n $minimum_system ]]

output_root="${1:-$PWD/dist/macos.noindex}"
mkdir -p "$output_root"
output_root=$(cd "$output_root" && pwd -P)
output=$(mktemp -d "$output_root/StreamTube-$version-$(uname -m).XXXXXXXX")
app="$output/$app_name.app"
iconset="$output/StreamTube.iconset"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$iconset"
install -m 755 "$binary" "$app/Contents/MacOS/streamtube"
install -m 644 LICENSE "$app/Contents/Resources/LICENSE"
for size in 16 32 128 256; do
    sips -z "$size" "$size" assets/logo.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" assets/logo.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/StreamTube.icns"
rm -r "$iconset"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>streamtube</string>
    <key>CFBundleIconFile</key>
    <string>StreamTube.icns</string>
    <key>CFBundleIdentifier</key>
    <string>$bundle_id</string>
    <key>CFBundleName</key>
    <string>$app_name</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version%%-*}</string>
    <key>CFBundleVersion</key>
    <string>${version%%-*}</string>
    <key>LSMinimumSystemVersion</key>
    <string>$minimum_system</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.entertainment</string>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>CFBundleURLName</key>
            <string>io.github.avaneesh-inamdar.StreamTube.link</string>
            <key>CFBundleURLSchemes</key>
            <array><string>$url_scheme</string></array>
        </dict>
    </array>
</dict>
</plist>
EOF

signing_identity=-
if [[ $safari_prototype == 1 ]]; then
    signing_identity=${STREAMTUBE_SAFARI_SIGNING_IDENTITY:--}
    bash packaging/macos/safari-history/embed-extension.sh "$app" "$signing_identity" "$bundle_id" "$safari_test_id"
fi
plutil -lint "$app/Contents/Info.plist" >&2
codesign --force --sign "$signing_identity" "$app" >&2
codesign --verify --strict "$app"
printf '%s\n' "$app"

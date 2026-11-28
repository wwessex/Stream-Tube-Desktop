#!/bin/bash
set -euo pipefail

source_dir=$(cd "$(dirname "$0")" && pwd -P)
app=$1
identity=$2
bundle_id=$3
test_id=$4
name="Stream Tube Safari History"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/streamtube-safari-extension.XXXXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/project"

web_dir="$source_dir/web"
if [[ -n $test_id ]]; then
    web_dir="$scratch/web"
    ditto "$source_dir/web" "$web_dir"
    display_name="Stream Tube Watch History ($test_id)"
    plutil -replace name -string "$display_name" "$web_dir/manifest.json"
    sed -i '' "s/io\.github\.avaneesh-inamdar\.StreamTube\.SafariPrototype/$bundle_id/g" \
        "$web_dir/background.js" "$web_dir/connect.js"
fi

xcrun safari-web-extension-converter "$web_dir" --project-location "$scratch/project" \
    --app-name "$name" --bundle-identifier "$bundle_id" --swift --macos-only --no-open --no-prompt >&2
project="$scratch/project/$name"
cp "$source_dir/SafariWebExtensionHandler.swift" "$project/$name Extension/"
xcodebuild -project "$project/$name.xcodeproj" -target "$name Extension" \
    -configuration Release -arch arm64 CONFIGURATION_BUILD_DIR="$scratch/products" CODE_SIGNING_ALLOWED=NO \
    > "$scratch/build.log" 2>&1 || { cat "$scratch/build.log" >&2; exit 1; }

mkdir -p "$app/Contents/PlugIns"
extension="$app/Contents/PlugIns/$name Extension.appex"
ditto "$scratch/products/$name Extension.appex" "$extension"
if [[ -n $test_id ]]; then
    plutil -replace CFBundleDisplayName -string "$display_name" "$extension/Contents/Info.plist"
fi
codesign --force --sign "$identity" --entitlements "$source_dir/entitlements.plist" "$extension" >&2

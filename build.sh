#!/bin/bash
set -euo pipefail

install_app=false
case "${1:-}" in
  "") ;;
  --install) install_app=true ;;
  -h|--help)
    printf 'Usage: %s [--install]\nBuild OptChat.app for this Mac; optionally install in ~/Applications.\n' "$0"
    exit 0 ;;
  *) printf 'Unknown option: %s\n' "$1" >&2; exit 2 ;;
esac
if (( $# > 1 )); then
  printf 'Expected at most one option.\n' >&2
  exit 2
fi
if [[ "$(uname -s)" != Darwin ]]; then
  printf 'This script builds macOS apps and must run on macOS.\n' >&2
  exit 1
fi
for command in cargo codesign plutil ditto; do
  command -v "$command" >/dev/null || { printf 'Missing required command: %s\n' "$command" >&2; exit 1; }
done

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$project_dir"
bundle="$project_dir/target/macos/OptChat.app"
host_target="$(rustc -vV | awk '/^host:/ {print $2}')"
version="$(awk -F '"' '/^version = / {print $2; exit}' Cargo.toml)"
cargo build --release --locked --target "$host_target" --target-dir "$project_dir/target"

mkdir -p "$bundle/Contents/MacOS"
cp "$project_dir/target/$host_target/release/optchat" "$bundle/Contents/MacOS/optchat"
cat > "$bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>optchat</string>
  <key>CFBundleIdentifier</key><string>local.optchat.desktop</string>
  <key>CFBundleName</key><string>OptChat</string>
  <key>CFBundleDisplayName</key><string>OptChat</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
plutil -lint "$bundle/Contents/Info.plist"
codesign --force --sign - "$bundle"
codesign --verify --strict "$bundle"
printf '\nBuilt: %s\n' "$bundle"

if "$install_app"; then
  destination="$HOME/Applications/OptChat.app"
  if [[ -e "$destination" ]]; then
    printf 'An app already exists at %s. Move it aside before installing this build.\n' "$destination" >&2
    exit 1
  fi
  mkdir -p "$HOME/Applications"
  ditto "$bundle" "$destination"
  codesign --verify --strict "$destination"
  printf 'Installed: %s\nLaunch with: open "%s"\n' "$destination" "$destination"
fi

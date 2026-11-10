#!/usr/bin/env bash
# macOS 签名 + 公证发布脚本。
#
# 前置条件（需在 Apple 侧完成，仓库内无法代办）：
#   1. Apple Developer Program 账号，创建 "Developer ID Application" 证书并安装到钥匙串。
#   2. 公证凭据二选一：
#      - Apple ID：APPLE_ID + APPLE_PASSWORD（App 专用密码）+ APPLE_TEAM_ID
#      - App Store Connect API Key：APPLE_API_ISSUER + APPLE_API_KEY + APPLE_API_KEY_PATH
#
# 用法：
#   npm run sidecar:build:release   # 可选；tauri build 的 beforeBuildCommand 也会执行
#   ./scripts/macos-release.sh
#
# 本机当前没有 Developer ID 证书时，脚本会在第一步直接退出，
# 不会生成任何“看起来已签名/已公证”的产物。
set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
    echo "错误：$*" >&2
    exit 1
}

[[ "$(uname -s)" == "Darwin" ]] || fail "只能在 macOS 上执行"

SIGN_CONF="src-tauri/tauri.signing.conf.json"
APP="src-tauri/target/release/bundle/macos/RustLocalSQLTool.app"
DMG_DIR="src-tauri/target/release/bundle/dmg"

if ! security find-identity -v -p codesigning | grep -q "Developer ID Application"; then
    fail "钥匙串中没有 Developer ID Application 证书。请先在 Apple Developer 后台创建并安装证书；在此之前无法签名或公证。"
fi

if grep -q "<YOUR NAME>\|<TEAMID>" "$SIGN_CONF"; then
    fail "src-tauri/tauri.signing.conf.json 仍是占位身份。请把 <YOUR NAME> / <TEAMID> 替换为证书上的真实值。"
fi

if [[ -z "${APPLE_API_KEY:-}" ]]; then
    [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]] || \
        fail "缺少公证凭据：设置 APPLE_ID + APPLE_PASSWORD（App 专用密码）+ APPLE_TEAM_ID，或 APPLE_API_ISSUER + APPLE_API_KEY + APPLE_API_KEY_PATH。"
else
    [[ -n "${APPLE_API_ISSUER:-}" && -n "${APPLE_API_KEY_PATH:-}" ]] || \
        fail "使用 API Key 公证时需要 APPLE_API_ISSUER 与 APPLE_API_KEY_PATH。"
fi

echo "==> 构建并签名（hardened runtime + entitlements）"
npx --no-install tauri build --config "$SIGN_CONF"

[[ -d "$APP" ]] || fail "未找到 $APP"

echo "==> 校验签名"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dv --verbose=4 "$APP" 2>&1 | grep -E "Authority|TeamIdentifier|Timestamp|Runtime Version" || true

echo "==> Gatekeeper 评估（未公证时会失败，这是预期结果）"
spctl -a -vv "$APP" || echo "提示：spctl 未通过，请检查公证票据是否已装订。"

echo "==> 校验公证票据（以 DMG 为准）"
shopt -s nullglob
dmg_found=0
for f in "$DMG_DIR"/*.dmg; do
    dmg_found=1
    xcrun stapler validate "$f"
done
[[ "$dmg_found" == "1" ]] || fail "未找到 DMG 产物，无法确认公证票据。"
xcrun stapler validate "$APP" || echo "提示：.app 未装订票据；部分 Tauri 版本只对 DMG 装订，请以 DMG 校验结果为准。"

echo "完成：已签名并通过公证票据校验。"

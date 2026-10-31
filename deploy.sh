#!/usr/bin/env bash
# One-command deploy: builds the app, pushes source, publishes a GitHub release
# with the .dmg, and serves docs/ on GitHub Pages.
#
#   ./deploy.sh
#   REPO_NAME=foo VISIBILITY=private ./deploy.sh   # first run only
set -euo pipefail

ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"

REPO_NAME=${REPO_NAME:-$(basename "$ROOT")}
VISIBILITY=${VISIBILITY:-public}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TAG=v$VERSION
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}

gh auth status >/dev/null 2>&1 || { echo "run 'gh auth login' first" >&2; exit 1; }

export SIGN_IDENTITY=${SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -1)}
if [ -z "$SIGN_IDENTITY" ]; then
    echo "no 'Developer ID Application' certificate in the keychain (Xcode > Settings > Accounts > Manage Certificates)" >&2
    exit 1
fi
NOTARY_PROFILE=${NOTARY_PROFILE:-imagesmith}
if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
    echo "no notary credentials; run once: xcrun notarytool store-credentials $NOTARY_PROFILE" >&2
    exit 1
fi

[ -d .git ] || git init -b main
BRANCH=$(git symbolic-ref --short HEAD)

if git remote get-url origin >/dev/null 2>&1; then
    REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
else
    REPO=$(gh api user -q .login)/$REPO_NAME
fi

echo "==> site"
[ -f docs/icon.png ] || swift bundle/make-icon.swift docs/icon.png
sed -i '' "s#data-repo=\"[^\"]*\"#data-repo=\"$REPO\"#" docs/index.html

echo "==> commit"
git add -A
git diff --cached --quiet || git commit -m "Release $TAG"

if ! git remote get-url origin >/dev/null 2>&1; then
    echo "==> creating $VISIBILITY repo $REPO"
    gh repo create "$REPO" "--$VISIBILITY" --source . --remote origin --push
else
    git push -u origin "$BRANCH"
fi

echo "==> build"
bundle/make-app.sh --dmg
DMG=$TARGET_DIR/release/ImageSmith.dmg
cp "$TARGET_DIR/release/ImageSmith-$VERSION.dmg" "$DMG"

echo "==> notarize"
xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$DMG"

echo "==> release $TAG"
if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    gh release upload "$TAG" "$DMG" -R "$REPO" --clobber
else
    gh release create "$TAG" "$DMG" -R "$REPO" --target "$BRANCH" --title "ImageSmith $VERSION" --generate-notes
fi

echo "==> pages"
if ! gh api "repos/$REPO/pages" >/dev/null 2>&1; then
    gh api -X POST "repos/$REPO/pages" -f "source[branch]=$BRANCH" -f "source[path]=/docs" >/dev/null
fi

echo
echo "repo:    https://github.com/$REPO"
echo "release: https://github.com/$REPO/releases/tag/$TAG"
echo "site:    $(gh api "repos/$REPO/pages" -q .html_url)"

#!/usr/bin/env bash
# Add .deb packages to a signed APT repository and regenerate its indexes.
#
#   packaging/apt-repo.sh <repo-dir> <package.deb>...
#
# Layout: <repo-dir>/pool/main/x/xl/*.deb and <repo-dir>/dists/stable/...
# Env:
#   GPG_KEY_ID  key used to sign the Release file (must be in the keyring)
#   KEEP        versions to keep per architecture (default 5)
#   SITE_URL    public URL of the repository (for the generated index.html)
set -euo pipefail

REPO=${1:?usage: apt-repo.sh <repo-dir> <deb>...}
shift
: "${GPG_KEY_ID:?set GPG_KEY_ID to the signing key}"
KEEP=${KEEP:-5}
SITE_URL=${SITE_URL:-https://yeager620.github.io/xl}

POOL="$REPO/pool/main/x/xl"
mkdir -p "$POOL"
for deb in "$@"; do
  cp "$deb" "$POOL/"
done

# Keep only the newest $KEEP versions of each architecture.
for arch in $(find "$POOL" -name '*.deb' | sed -E 's/.*_([a-z0-9]+)\.deb$/\1/' | sort -u); do
  find "$POOL" -name "*_${arch}.deb" | sort -V | head -n "-$KEEP" | xargs -r rm -f
done

cd "$REPO"
ARCHES=$(find pool -name '*.deb' | sed -E 's/.*_([a-z0-9]+)\.deb$/\1/' | sort -u | tr '\n' ' ')
rm -rf dists
for arch in $ARCHES; do
  dir="dists/stable/main/binary-$arch"
  mkdir -p "$dir"
  apt-ftparchive --arch "$arch" packages pool > "$dir/Packages"
  gzip -9nkf "$dir/Packages"
done

apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=xl \
  -o APT::FTPArchive::Release::Label=xl \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Components=main \
  -o APT::FTPArchive::Release::Architectures="${ARCHES% }" \
  -o APT::FTPArchive::Release::Description="xl spreadsheet" \
  release dists/stable > dists/stable/Release.tmp
mv dists/stable/Release.tmp dists/stable/Release

gpg --batch --yes --local-user "$GPG_KEY_ID" --clearsign -o dists/stable/InRelease dists/stable/Release
gpg --batch --yes --local-user "$GPG_KEY_ID" --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
gpg --batch --yes --export "$GPG_KEY_ID" > xl-archive-keyring.gpg

cat > index.html <<HTML
<!doctype html>
<meta charset="utf-8">
<title>xl APT repository</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:46rem;margin:3rem auto;padding:0 1rem;color:#1e2024}pre{background:#f1f2f4;padding:1rem;overflow-x:auto}</style>
<h1>xl APT repository</h1>
<p>Install <a href="https://github.com/yeager620/xl">xl</a>, a fast native spreadsheet, on Debian or Ubuntu:</p>
<pre>curl -fsSL $SITE_URL/xl-archive-keyring.gpg | sudo tee /usr/share/keyrings/xl-archive-keyring.gpg &gt;/dev/null
echo "deb [signed-by=/usr/share/keyrings/xl-archive-keyring.gpg] $SITE_URL stable main" | sudo tee /etc/apt/sources.list.d/xl.list
sudo apt update
sudo apt install xl</pre>
<p>Updates arrive with <code>sudo apt upgrade</code>.</p>
HTML
touch .nojekyll
echo "Repository updated: $(find pool -name '*.deb' | wc -l) package(s), architectures: $ARCHES"

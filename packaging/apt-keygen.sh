#!/usr/bin/env bash
# One-time setup: create the key that signs the APT repository and store it
# as the APT_SIGNING_KEY secret for the release workflow.
#
# The key has no passphrase (CI must use it unattended) and no email address.
# Back up the private key somewhere safe and offline; anyone with it can sign
# packages that users' apt will trust.
set -euo pipefail
REPO=${1:-yeager620/xl}
NAME="xl package signing"

if ! gpg --list-secret-keys "$NAME" >/dev/null 2>&1; then
  gpg --batch --passphrase '' --quick-gen-key "$NAME" ed25519 sign 5y
fi
FPR=$(gpg --list-secret-keys --with-colons "$NAME" | awk -F: '/^fpr/{print $10; exit}')
gpg --armor --export-secret-keys "$FPR" | gh secret set APT_SIGNING_KEY --repo "$REPO"
echo
echo "Signing key $FPR is stored as the APT_SIGNING_KEY secret on $REPO."
echo "Back it up (and keep the file offline):"
echo "  gpg --armor --export-secret-keys $FPR > xl-apt-signing-key.asc"

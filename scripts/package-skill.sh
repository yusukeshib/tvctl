#!/bin/sh
# Builds dist/tvctl-skill.zip (Agent Skills format: tvctl/SKILL.md) for agents
# that install skills from a zip.
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
rm -f dist/tvctl-skill.zip
(cd skills && zip -qr ../dist/tvctl-skill.zip tvctl)
echo "dist/tvctl-skill.zip"
